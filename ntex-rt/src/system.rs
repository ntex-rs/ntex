use std::any::{Any, TypeId};
use std::collections::VecDeque;
use std::sync::{Arc, atomic::AtomicBool, atomic::AtomicUsize, atomic::Ordering};
use std::time::{Duration, Instant};
use std::{cell::RefCell, fmt, future::Future, panic, pin::Pin, rc::Rc};

use async_channel::{Receiver, Sender, unbounded};
use futures_timer::Delay;
use parking_lot::{Mutex, RwLock};

use crate::arbiter::Arbiter;
use crate::pool::ThreadPool;
use crate::{BlockingResult, Builder, Handle, HashMap, HashSet, Runner, SystemRunner};

static SYSTEM_COUNT: AtomicUsize = AtomicUsize::new(0);

thread_local!(
    static PINGS: RefCell<HashMap<Id, VecDeque<PingRecord>>> =
        RefCell::new(HashMap::default());
);

#[derive(Default)]
struct Arbiters {
    all: HashMap<Id, Arbiter>,
    list: Vec<Arbiter>,
}

/// System id
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Id(pub(crate) usize);

/// System is a runtime manager
pub struct System(Arc<SystemInner>);

struct SystemInner {
    id: usize,
    arbiter: Arbiter,
    config: SystemConfig,
    sender: Sender<SystemCommand>,
    receiver: Receiver<SystemCommand>,
    storage: RwLock<HashMap<TypeId, Box<dyn Any + Sync + Send>>>,
    arbiters: Mutex<Arbiters>,
    signals: AtomicBool,
    pool: ThreadPool,
}

#[derive(Clone)]
pub struct SystemConfig {
    pub(super) name: String,
    pub(super) stack_size: usize,
    pub(super) stop_on_panic: bool,
    pub(super) ping_interval: usize,
    pub(super) pool_limit: usize,
    pub(super) pool_recv_timeout: Duration,
    pub(super) testing: bool,
    pub(super) runner: Arc<dyn Runner>,
}

thread_local!(
    static CURRENT: RefCell<Option<System>> = const { RefCell::new(None) };
);

impl Clone for System {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl System {
    /// Constructs new system and sets it as current
    pub(super) fn start(config: SystemConfig) -> (Self, oneshot::Receiver<i32>) {
        let id = SYSTEM_COUNT.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = unbounded();

        let pool =
            ThreadPool::new(&config.name, config.pool_limit, config.pool_recv_timeout);
        let (arbiter, controller) = Arbiter::new_system(id, config.name.clone());

        let mut arbiters = Arbiters::default();
        arbiters.all.insert(arbiter.id(), arbiter.clone());
        arbiters.list.push(arbiter.clone());

        let sys = System(Arc::new(SystemInner {
            id,
            config,
            arbiter,
            sender,
            receiver,
            pool,
            arbiters: Mutex::new(arbiters),
            storage: RwLock::new(HashMap::default()),
            signals: AtomicBool::new(false),
        }));
        System::set_current(sys.clone());

        let (stop_tx, stop) = oneshot::channel();

        // system support tasks
        crate::spawn(SystemSupport::new(&sys, stop_tx).run());
        crate::spawn(controller.run(sys.clone()));

        (sys, stop)
    }

    /// Build a new system with a customized runtime
    ///
    /// This allows to customize the runtime. See struct level docs on
    /// `Builder` for more information.
    pub fn build() -> Builder {
        Builder::new()
    }

    #[allow(clippy::new_ret_no_self)]
    /// Create new system
    ///
    /// This method panics if it can not create runtime
    pub fn new<R: Runner>(name: &str, runner: R) -> SystemRunner {
        Self::build().name(name).build(runner)
    }

    #[allow(clippy::new_ret_no_self)]
    /// Create new system
    ///
    /// This method panics if it can not create runtime
    pub fn with_config(name: &str, config: SystemConfig) -> SystemRunner {
        Self::build().name(name).build_with(config)
    }

    /// Get current running system
    ///
    /// # Panics
    ///
    /// Panics if System is not running
    pub fn current() -> System {
        CURRENT.with(|cell| match *cell.borrow() {
            Some(ref sys) => sys.clone(),
            None => panic!("System is not running"),
        })
    }

    /// Runs a function using the system context.
    pub fn try_current() -> Option<System> {
        CURRENT.with(|cell| cell.borrow().as_ref().map(Clone::clone))
    }

    /// Set current running system
    #[doc(hidden)]
    pub fn set_current(sys: System) {
        CURRENT.with(|s| {
            *s.borrow_mut() = Some(sys);
        });
    }

    pub(crate) fn register_arbiter(&self, arb: Arbiter) {
        CURRENT.with(|s| {
            *s.borrow_mut() = Some(self.clone());
        });
        let mut arbiters = self.0.arbiters.lock();
        arbiters.all.insert(arb.id(), arb.clone());
        arbiters.list.push(arb);
    }

    pub(crate) fn unregister_arbiter(&self, id: Id) {
        CURRENT.with(|s| {
            *s.borrow_mut() = None;
        });
        let mut arbiters = self.0.arbiters.lock();
        if let Some(hnd) = arbiters.all.remove(&id) {
            for (idx, arb) in arbiters.list.iter().enumerate() {
                if &hnd == arb {
                    arbiters.list.remove(idx);
                    break;
                }
            }
        }
    }

    pub(super) fn remove_current() {
        CURRENT.with(|cell| {
            cell.borrow_mut().take();
        });
    }

    /// System id
    pub fn id(&self) -> Id {
        Id(self.0.id)
    }

    /// System name
    pub fn name(&self) -> &str {
        &self.0.config.name
    }

    /// Stop the system
    pub fn stop(&self) {
        self.stop_with_code(0);
    }

    /// Stop the system with a particular exit code
    pub fn stop_with_code(&self, code: i32) {
        let _ = self.0.sender.try_send(SystemCommand::Exit(code));
    }

    /// Return status of `stop_on_panic` option
    ///
    /// It controls whether the System is stopped when an
    /// uncaught panic is thrown from a worker thread.
    pub fn stop_on_panic(&self) -> bool {
        self.0.config.stop_on_panic
    }

    /// Return status of `signals` option
    pub fn signals(&self) -> bool {
        self.0.signals.load(Ordering::Relaxed)
    }

    /// Enable `signals` handling
    pub fn enable_signals(&self) {
        if !self.signals() {
            crate::signals::start(self);
            self.0.signals.store(true, Ordering::Relaxed);
        }
    }

    /// Disable `signals` handling
    pub fn disable_signals(&self) {
        if self.signals() {
            crate::signals::stop(self);
            self.0.signals.store(false, Ordering::Relaxed);
        }
    }

    /// System arbiter
    ///
    /// # Panics
    ///
    /// Panics if system is not started
    pub fn arbiter(&self) -> Arbiter {
        self.0.arbiter.clone()
    }

    /// Retrieves a list of all arbiters in the system
    ///
    /// This method should be called from the thread where the system has been initialized,
    /// typically the "main" thread.
    pub fn list_arbiters<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&[Arbiter]) -> R,
    {
        f(&self.0.arbiters.lock().list)
    }

    /// Retrieves a list of last pings records for specified arbiter
    ///
    /// This method should be called from the thread where the system has been initialized,
    /// typically the "main" thread.
    pub fn list_arbiter_pings<F, R>(id: Id, f: F) -> R
    where
        F: FnOnce(Option<&VecDeque<PingRecord>>) -> R,
    {
        PINGS.with(|pings| {
            if let Some(recs) = pings.borrow().get(&id) {
                f(Some(recs))
            } else {
                f(None)
            }
        })
    }

    #[cfg(target_os = "linux")]
    #[doc(hidden)]
    /// Set arbiter latency callback.
    ///
    /// This callback is called when the arbiter response latency exceeds the
    /// configured threshold. The provided backtrace is not resolved.
    ///
    /// Note: This callback is not thread-safe.
    pub fn set_latency_callback<F: Fn(ntex_error::Backtrace) + 'static>(f: F) {
        unsafe {
            ARB_CB = Some(Box::new(f));
        }
    }

    /// System config
    pub fn config(&self) -> SystemConfig {
        self.0.config.clone()
    }

    #[inline]
    /// Runtime handle for main thread
    pub fn handle(&self) -> Handle {
        self.arbiter().handle().clone()
    }

    /// Testing flag
    pub fn testing(&self) -> bool {
        self.0.config.testing()
    }

    /// Spawns a blocking task in a new thread, and wait for it
    ///
    /// The task will not be cancelled even if the future is dropped.
    pub fn spawn_blocking<F, R>(&self, f: F) -> BlockingResult<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        self.0.pool.execute(f)
    }

    /// Returns a previously registered type, or inserts and returns a new one.
    ///
    /// This method acquires a lock on the internal data structure.
    /// To avoid repeated locking, prefer storing a cloned value in the arbiter's storage.
    pub fn get_value<T>(&self, f: impl FnOnce() -> T) -> T
    where
        T: Clone + Send + Sync + 'static,
    {
        if let Some(boxed) = self.0.storage.read().get(&TypeId::of::<T>())
            && let Some(val) = (&**boxed as &(dyn Any + 'static)).downcast_ref::<T>()
        {
            val.clone()
        } else {
            let val = f();
            self.0
                .storage
                .write()
                .insert(TypeId::of::<T>(), Box::new(val.clone()));
            val
        }
    }
}

impl SystemConfig {
    #[inline]
    /// Is current system is testing
    pub fn testing(&self) -> bool {
        self.testing
    }
}

impl fmt::Debug for System {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("System")
            .field("id", &self.0.id)
            .field("config", &self.0.config)
            .field("signals", &self.signals())
            .field("pool", &self.0.pool)
            .finish()
    }
}

impl fmt::Debug for SystemConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SystemConfig")
            .field("name", &self.name)
            .field("testing", &self.testing)
            .field("stack_size", &self.stack_size)
            .field("stop_on_panic", &self.stop_on_panic)
            .finish()
    }
}

#[derive(Debug)]
pub(super) enum SystemCommand {
    Exit(i32),
}

#[derive(Debug)]
struct SystemSupport {
    sys: System,
    stop: Option<oneshot::Sender<i32>>,
    commands: Receiver<SystemCommand>,
    ping_interval: Duration,
}

impl SystemSupport {
    fn new(sys: &System, stop: oneshot::Sender<i32>) -> Self {
        Self {
            sys: sys.clone(),
            stop: Some(stop),
            commands: sys.0.receiver.clone(),
            ping_interval: Duration::from_millis(sys.0.config.ping_interval as u64),
        }
    }

    async fn run(mut self) {
        crate::spawn(ping_arbiters(self.sys.clone(), self.ping_interval));

        loop {
            match self.commands.recv().await {
                Ok(SystemCommand::Exit(code)) => {
                    log::debug!("Stopping system with {code} code");

                    // stop arbiters
                    let mut arbiters = self.sys.0.arbiters.lock();
                    for arb in arbiters.list.drain(..) {
                        arb.stop();
                    }
                    arbiters.all.clear();

                    // stop event loop
                    if let Some(stop) = self.stop.take() {
                        let _ = stop.send(code);
                    }
                }
                Err(_) => {
                    log::debug!("System stopped");
                    return;
                }
            }
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub struct PingRecord {
    /// Ping start time
    pub start: Instant,
    /// Round-trip time, if value is not set then ping is in process
    pub rtt: Option<Duration>,
}

async fn ping_arbiters(sys: System, interval: Duration) {
    let pings = Rc::new(RefCell::new(HashSet::default()));

    loop {
        // send pings
        {
            pings.borrow_mut().clear();

            let start = Instant::now();
            let arbiters = sys.0.arbiters.lock();

            for arb in &arbiters.list {
                let id = arb.id();
                let pings = pings.clone();
                let fut = arb.handle().spawn(async move {
                    yield_to().await;
                });

                // calc ttl
                PINGS.with(|pings| {
                    let mut p = pings.borrow_mut();
                    let recs = p.entry(arb.id()).or_default();
                    recs.push_front(PingRecord { start, rtt: None });
                    recs.truncate(10);
                });

                crate::spawn(async move {
                    if fut.await.is_ok() {
                        pings.borrow_mut().insert(id);

                        PINGS.with(|pings| {
                            pings
                                .borrow_mut()
                                .get_mut(&id)
                                .unwrap()
                                .front_mut()
                                .unwrap()
                                .rtt = Some(start.elapsed());
                        });
                    }
                });
            }
        }

        Delay::new(interval).await;

        // check pings
        #[cfg(target_os = "linux")]
        {
            const SPIN: Duration = Duration::from_micros(100);

            let mut no_pongs = Vec::new();

            {
                for arb in &sys.0.arbiters.lock().list {
                    let pong = pings.borrow_mut().remove(&arb.id());
                    if !pong {
                        no_pongs.push(arb.clone());
                    }
                }
            }

            for arb in no_pongs {
                // no response from arbiter
                log::error!("Arbiter {}({:?}) did not return pong", arb.name(), arb.id());

                // send tgkill to thread id to capture backtrace
                *CAPTURED.lock() = None;
                EXPECTED_TID.store(arb.tid(), Ordering::Release);
                unsafe {
                    libc::syscall(
                        libc::SYS_tgkill,
                        libc::getpid(),
                        arb.tid(),
                        libc::SIGUSR2,
                    );
                }

                // Spin
                for _ in 0..1000 {
                    Delay::new(SPIN).await;
                    if let Some(bt) = CAPTURED.lock().take() {
                        let bt = ntex_error::Backtrace::from(bt);
                        #[allow(static_mut_refs)]
                        if let Some(f) = unsafe { ARB_CB.as_ref() } {
                            f(bt);
                        } else {
                            bt.resolver().resolve();
                            log::error!(
                                "Worker does not returned pong within {interval:?} time.\n{bt:?}"
                            );
                        }
                        break;
                    }
                }
            }
        }
    }
}

async fn yield_to() {
    use std::task::{Context, Poll};

    struct Yield {
        completed: bool,
    }

    impl Future for Yield {
        type Output = ();

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            if self.completed {
                return Poll::Ready(());
            }
            self.completed = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }

    Yield { completed: false }.await;
}

#[cfg(target_os = "linux")]
static mut ARB_CB: Option<Box<dyn Fn(ntex_error::Backtrace)>> = None;

#[cfg(target_os = "linux")]
static EXPECTED_TID: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
#[cfg(target_os = "linux")]
static CAPTURED: Mutex<Option<ntex_error::BacktraceRaw>> = Mutex::new(None);

#[track_caller]
#[cfg(target_family = "unix")]
pub(crate) fn sig_usr2() {
    #[cfg(target_os = "linux")]
    #[allow(clippy::cast_possible_truncation)]
    {
        let tid = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
        if EXPECTED_TID.load(Ordering::Acquire) == tid {
            // backtrace::Backtrace::new_unresolved uses libunwind frame walking,
            // which is signal-safe. Symbol resolution is NOT — do it later.
            let bt = ntex_error::BacktraceRaw::new(panic::Location::caller());
            *CAPTURED.lock() = Some(bt);
        }
    }
}
