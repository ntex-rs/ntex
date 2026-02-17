use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, RwLock, atomic::AtomicUsize, atomic::Ordering};
use std::time::{Duration, Instant};
use std::{cell::Cell, cell::RefCell, fmt, future::Future, panic, pin::Pin};

use async_channel::{Receiver, Sender, unbounded};
use futures_timer::Delay;

use crate::pool::ThreadPool;
use crate::{Arbiter, BlockingResult, Builder, Handle, Runner, SystemRunner};

static SYSTEM_COUNT: AtomicUsize = AtomicUsize::new(0);

thread_local!(
    static ARBITERS: RefCell<Arbiters> = RefCell::new(Arbiters::default());
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
pub struct System {
    id: usize,
    arbiter: Cell<Option<Arbiter>>,
    config: SystemConfig,
    sender: Sender<SystemCommand>,
    receiver: Receiver<SystemCommand>,
    rt: Arc<RwLock<Arbiter>>,
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
        Self {
            id: self.id,
            arbiter: Cell::new(None),
            config: self.config.clone(),
            sender: self.sender.clone(),
            receiver: self.receiver.clone(),
            rt: self.rt.clone(),
            pool: self.pool.clone(),
        }
    }
}

impl System {
    /// Constructs new system and sets it as current
    pub(super) fn construct(config: SystemConfig) -> Self {
        let id = SYSTEM_COUNT.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = unbounded();

        let pool =
            ThreadPool::new(&config.name, config.pool_limit, config.pool_recv_timeout);

        System {
            id,
            config,
            sender,
            receiver,
            pool,
            rt: Arc::new(RwLock::new(Arbiter::dummy())),
            arbiter: Cell::new(None),
        }
    }

    /// Constructs new system and sets it as current
    pub(super) fn start(&mut self) -> oneshot::Receiver<i32> {
        let (stop_tx, stop) = oneshot::channel();
        let (arb, controller) = Arbiter::new_system(self.id, self.config.name.clone());

        self.arbiter.set(Some(arb.clone()));
        *self.rt.write().unwrap() = arb.clone();
        System::set_current(self.clone());

        // system support tasks
        crate::spawn(SystemSupport::new(self, stop_tx).run(arb.id(), arb));
        crate::spawn(controller.run());

        stop
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

    /// Set current running system
    #[doc(hidden)]
    pub fn set_current(sys: System) {
        sys.arbiter().set_current();
        CURRENT.with(|s| {
            *s.borrow_mut() = Some(sys);
        });
    }

    /// System id
    pub fn id(&self) -> Id {
        Id(self.id)
    }

    /// System name
    pub fn name(&self) -> &str {
        &self.config.name
    }

    /// Stop the system
    pub fn stop(&self) {
        self.stop_with_code(0);
    }

    /// Stop the system with a particular exit code
    pub fn stop_with_code(&self, code: i32) {
        let _ = self.sender.try_send(SystemCommand::Exit(code));
    }

    /// Return status of `stop_on_panic` option
    ///
    /// It controls whether the System is stopped when an
    /// uncaught panic is thrown from a worker thread.
    pub fn stop_on_panic(&self) -> bool {
        self.config.stop_on_panic
    }

    /// System arbiter
    pub fn arbiter(&self) -> Arbiter {
        if let Some(arb) = self.arbiter.take() {
            self.arbiter.set(Some(arb.clone()));
            if arb.hnd.is_some() {
                return arb;
            }
        }

        let arb = self.rt.read().unwrap().clone();
        self.arbiter.set(Some(arb.clone()));
        arb
    }

    /// Retrieves a list of all arbiters in the system
    ///
    /// This method should be called from the thread where the system has been initialized,
    /// typically the "main" thread.
    pub fn list_arbiters<F, R>(f: F) -> R
    where
        F: FnOnce(&[Arbiter]) -> R,
    {
        ARBITERS.with(|arbs| f(arbs.borrow().list.as_ref()))
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

    pub(super) fn sys(&self) -> &Sender<SystemCommand> {
        &self.sender
    }

    /// System config
    pub fn config(&self) -> SystemConfig {
        self.config.clone()
    }

    #[inline]
    /// Runtime handle for main thread
    pub fn handle(&self) -> Handle {
        self.arbiter().handle().clone()
    }

    /// Testing flag
    pub fn testing(&self) -> bool {
        self.config.testing()
    }

    /// Spawns a blocking task in a new thread, and wait for it
    ///
    /// The task will not be cancelled even if the future is dropped.
    pub fn spawn_blocking<F, R>(&self, f: F) -> BlockingResult<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        self.pool.dispatch(f)
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
            .field("id", &self.id)
            .field("config", &self.config)
            .field("pool", &self.pool)
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
    RegisterArbiter(Id, Arbiter),
    UnregisterArbiter(Id),
}

#[derive(Debug)]
struct SystemSupport {
    stop: Option<oneshot::Sender<i32>>,
    commands: Receiver<SystemCommand>,
    ping_interval: Duration,
}

impl SystemSupport {
    fn new(sys: &System, stop: oneshot::Sender<i32>) -> Self {
        Self {
            stop: Some(stop),
            commands: sys.receiver.clone(),
            ping_interval: Duration::from_millis(sys.config.ping_interval as u64),
        }
    }

    async fn run(mut self, id: Id, arb: Arbiter) {
        ARBITERS.with(move |arbs| {
            let mut arbiters = arbs.borrow_mut();
            arbiters.all.clear();
            arbiters.list.clear();

            // system arbiter
            arbiters.all.insert(id, arb.clone());
            arbiters.list.push(arb.clone());
            crate::spawn(ping_arbiter(arb, self.ping_interval));
        });

        loop {
            match self.commands.recv().await {
                Ok(SystemCommand::Exit(code)) => {
                    log::debug!("Stopping system with {code} code");

                    // stop arbiters
                    ARBITERS.with(move |arbs| {
                        let mut arbiters = arbs.borrow_mut();
                        for arb in arbiters.list.drain(..) {
                            arb.stop();
                        }
                        arbiters.all.clear();
                    });

                    // stop event loop
                    if let Some(stop) = self.stop.take() {
                        let _ = stop.send(code);
                    }
                }
                Ok(SystemCommand::RegisterArbiter(id, hnd)) => {
                    crate::spawn(ping_arbiter(hnd.clone(), self.ping_interval));
                    ARBITERS.with(move |arbs| {
                        let mut arbiters = arbs.borrow_mut();
                        arbiters.all.insert(id, hnd.clone());
                        arbiters.list.push(hnd);
                    });
                }
                Ok(SystemCommand::UnregisterArbiter(id)) => {
                    ARBITERS.with(move |arbs| {
                        let mut arbiters = arbs.borrow_mut();
                        if let Some(hnd) = arbiters.all.remove(&id) {
                            for (idx, arb) in arbiters.list.iter().enumerate() {
                                if &hnd == arb {
                                    arbiters.list.remove(idx);
                                    break;
                                }
                            }
                        }
                    });
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

async fn ping_arbiter(arb: Arbiter, interval: Duration) {
    loop {
        Delay::new(interval).await;

        // check if arbiter is still active
        let is_alive = ARBITERS.with(|arbs| arbs.borrow().all.contains_key(&arb.id()));

        if !is_alive {
            PINGS.with(|pings| pings.borrow_mut().remove(&arb.id()));
            break;
        }

        // calc ttl
        let start = Instant::now();
        PINGS.with(|pings| {
            let mut p = pings.borrow_mut();
            let recs = p.entry(arb.id()).or_default();
            recs.push_front(PingRecord { start, rtt: None });
            recs.truncate(10);
        });

        let result = arb
            .handle()
            .spawn(async {
                yield_to().await;
            })
            .await;

        if result.is_err() {
            break;
        }

        PINGS.with(|pings| {
            pings
                .borrow_mut()
                .get_mut(&arb.id())
                .unwrap()
                .front_mut()
                .unwrap()
                .rtt = Some(start.elapsed());
        });
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

pub(super) trait FnExec: Send + 'static {
    fn call_box(self: Box<Self>);
}

impl<F> FnExec for F
where
    F: FnOnce() + Send + 'static,
{
    #[allow(clippy::boxed_local)]
    fn call_box(self: Box<Self>) {
        (*self)();
    }
}
