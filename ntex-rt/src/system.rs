use std::any::{Any, TypeId};
use std::collections::VecDeque;
use std::sync::{Arc, atomic::AtomicBool, atomic::AtomicUsize, atomic::Ordering};
use std::time::{Duration, Instant};
use std::{cell::RefCell, fmt, future::Future, panic, pin::Pin, sync::Weak, thread};

use async_channel::{Receiver, Sender, unbounded};
use futures_timer::Delay;
use parking_lot::{Mutex, RwLock};

use crate::arbiter::Arbiter;
#[cfg(target_os = "linux")]
use crate::capture::CAPTURE;
use crate::metrics::Stall;
use crate::pool::{PoolMetrics, ThreadPool};
use crate::{BlockingResult, Builder, Handle, HashMap, Runner, SystemRunner};

static SYSTEM_COUNT: AtomicUsize = AtomicUsize::new(0);

thread_local!(
    static PINGS: RefCell<HashMap<Id, VecDeque<PingRecord>>> = RefCell::new(HashMap::default());
);

#[derive(Default)]
struct Arbiters {
    all: HashMap<Id, Arbiter>,
    list: Vec<Arbiter>,
}

/// Identifier assigned to a running [`System`].
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Id(pub(crate) usize);

/// Runtime manager for a group of arbiter threads.
///
/// A system stores runtime configuration, manages arbiters, dispatches process
/// signals, and owns the blocking thread pool.
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

/// Configuration shared by a running [`System`].
#[derive(Clone)]
pub struct SystemConfig {
    pub(super) name: String,
    pub(super) stack_size: usize,
    pub(super) ping_interval: usize,
    pub(super) ping_threshold: usize,
    pub(super) latency_interval: usize,
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

        let pool = ThreadPool::new(&config.name, config.pool_limit, config.pool_recv_timeout);
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

    /// Creates a builder for a system with a customized runtime.
    ///
    /// See [`Builder`] for the available configuration options.
    pub fn build() -> Builder {
        Builder::new()
    }

    #[allow(clippy::new_ret_no_self)]
    /// Creates a system runner with the specified name and runtime runner.
    ///
    /// # Panics
    ///
    /// Panics if the runtime cannot be created.
    pub fn new<R: Runner>(name: &str, runner: R) -> SystemRunner {
        Self::build().name(name).build(runner)
    }

    #[allow(clippy::new_ret_no_self)]
    /// Creates a system runner from an existing configuration.
    ///
    /// # Panics
    ///
    /// Panics if the runtime cannot be created.
    pub fn with_config(name: &str, mut config: SystemConfig) -> SystemRunner {
        name.clone_into(&mut config.name);
        Self::build().name(name).build_with(config)
    }

    /// Returns the system running on the current thread.
    ///
    /// # Panics
    ///
    /// Panics if no system is running on the current thread.
    pub fn current() -> System {
        CURRENT.with(|cell| match *cell.borrow() {
            Some(ref sys) => sys.clone(),
            None => panic!("System is not running"),
        })
    }

    /// Returns the system running on the current thread, if one exists.
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

    /// Returns the system identifier.
    pub fn id(&self) -> Id {
        Id(self.0.id)
    }

    /// Returns the system name.
    pub fn name(&self) -> &str {
        &self.0.config.name
    }

    /// Stops the system with exit code `0`.
    pub fn stop(&self) {
        self.stop_with_code(0);
    }

    /// Stops the system with the specified exit code.
    pub fn stop_with_code(&self, code: i32) {
        let _ = self.0.sender.try_send(SystemCommand::Exit(code));
    }

    /// Returns whether process signal handling is enabled.
    pub fn signals(&self) -> bool {
        self.0.signals.load(Ordering::Relaxed)
    }

    /// Enables process signal handling.
    ///
    /// Signals are handled by one system at a time, this has no effect while
    /// another system handles signals.
    pub fn enable_signals(&self) {
        if !self.signals() && crate::signals::start(self) {
            self.0.signals.store(true, Ordering::Relaxed);
        }
    }

    /// Disables process signal handling.
    pub fn disable_signals(&self) {
        if self.signals() {
            crate::signals::stop(self);
            self.0.signals.store(false, Ordering::Relaxed);
        }
    }

    /// Returns the system's primary arbiter.
    ///
    /// # Panics
    ///
    /// Panics if the system has not been started.
    pub fn arbiter(&self) -> Arbiter {
        self.0.arbiter.clone()
    }

    /// Provides access to all arbiters registered with this system.
    ///
    /// This method should be called from the thread where the system has been initialized,
    /// typically the "main" thread.
    pub fn list_arbiters<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&[Arbiter]) -> R,
    {
        f(&self.0.arbiters.lock().list)
    }

    /// Visits the latest ping records for each registered arbiter.
    ///
    /// This method should be called from the thread where the system has been initialized,
    /// typically the "main" thread.
    pub fn list_arbiter_pings<F>(mut f: F)
    where
        F: FnMut(&Arbiter, &mut VecDeque<PingRecord>),
    {
        PINGS.with(|pings| {
            let mut p = pings.borrow_mut();
            let sys = System::current();
            let arbiters = sys.0.arbiters.lock();

            for (id, recs) in &mut *p {
                if let Some(arb) = arbiters.all.get(id) {
                    f(arb, recs);
                }
            }
        });
    }

    #[cfg(target_os = "linux")]
    #[doc(hidden)]
    /// Set arbiter latency callback.
    ///
    /// This callback is called when the arbiter response latency exceeds the
    /// configured threshold. The provided backtrace is not resolved.
    pub fn set_latency_callback<F>(f: F)
    where
        F: Fn(ntex_error::Backtrace) + Send + Sync + 'static,
    {
        *ARB_CB.lock() = Some(Arc::new(f));
    }

    /// Returns a clone of the system configuration.
    pub fn config(&self) -> SystemConfig {
        self.0.config.clone()
    }

    #[inline]
    /// Returns a runtime handle for the primary arbiter.
    pub fn handle(&self) -> Handle {
        self.arbiter().handle().clone()
    }

    /// Returns whether the system is configured for testing.
    pub fn testing(&self) -> bool {
        self.0.config.testing()
    }

    /// Spawns a blocking task on a new thread and waits for it to complete.
    ///
    /// Dropping the returned future prevents queued work from starting, but
    /// cannot interrupt work that is already running. Call
    /// [`BlockingResult::detach`] to let queued work continue even if its
    /// result is no longer needed.
    pub fn spawn_blocking<F, R>(&self, f: F) -> BlockingResult<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        self.0.pool.execute(f)
    }

    /// Returns a snapshot of the blocking thread pool metrics.
    pub fn pool_metrics(&self) -> PoolMetrics {
        self.0.pool.metrics()
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
    /// Returns whether the system is configured for testing.
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
    watchdog: Option<Arc<AtomicBool>>,
}

impl Drop for SystemSupport {
    fn drop(&mut self) {
        self.stop_watchdog();
    }
}

impl SystemSupport {
    fn new(sys: &System, stop: oneshot::Sender<i32>) -> Self {
        Self {
            sys: sys.clone(),
            stop: Some(stop),
            commands: sys.0.receiver.clone(),
            watchdog: None,
        }
    }

    fn stop_watchdog(&mut self) {
        if let Some(stop) = self.watchdog.take() {
            stop.store(true, Ordering::Relaxed);
        }
    }

    async fn run(mut self) {
        crate::metrics::start_latency_sampler(
            &self.sys.0.arbiter.0.latency,
            self.sys.0.config.latency_interval,
        );
        self.watchdog = start_watchdog(&self.sys);
        if self.sys.0.config.ping_interval != 0 {
            crate::spawn(ping_arbiters(self.sys.clone()));
        }

        loop {
            match self.commands.recv().await {
                Ok(SystemCommand::Exit(code)) => {
                    log::debug!("Stopping system with {code} code");
                    self.stop_watchdog();

                    // stop arbiters
                    let mut arbiters = self.sys.0.arbiters.lock();
                    for arb in arbiters.list.drain(..) {
                        arb.stop();
                    }
                    arbiters.all.clear();
                    drop(arbiters);

                    crate::arbiter::run_shutdown_callbacks();

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

async fn ping_arbiters(sys: System) {
    let interval = Duration::from_millis(sys.0.config.ping_interval as u64);

    loop {
        // interval between pings
        Delay::new(interval).await;

        let start = Instant::now();
        let arbiters = sys.0.arbiters.lock();

        // drop records of stopped arbiters
        PINGS.with(|pings| {
            pings
                .borrow_mut()
                .retain(|id, _| arbiters.all.contains_key(id));
        });

        for arb in &arbiters.list {
            let id = arb.id();
            let fut = arb.handle().spawn(async move {
                yield_to().await;
            });

            PINGS.with(|pings| {
                let mut p = pings.borrow_mut();
                let recs = p.entry(id).or_default();
                recs.push_front(PingRecord { start, rtt: None });
                recs.truncate(10);
            });

            crate::spawn(async move {
                if fut.await.is_ok() {
                    PINGS.with(|pings| {
                        // a late pong belongs to its own round
                        if let Some(recs) = pings.borrow_mut().get_mut(&id)
                            && let Some(rec) = recs.iter_mut().find(|r| r.start == start)
                        {
                            rec.rtt = Some(start.elapsed());
                        }
                    });
                }
            });
        }
    }
}

/// Starts a thread that detects stalled arbiters.
fn start_watchdog(sys: &System) -> Option<Arc<AtomicBool>> {
    let interval = sys.0.config.latency_interval;
    if interval == 0 {
        return None;
    }
    let interval = Duration::from_millis(interval as u64);
    let threshold = Duration::from_millis(sys.0.config.ping_threshold as u64);
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let sys = Arc::downgrade(&sys.0);

    let name = format!("{}:watchdog", sys.upgrade()?.config.name);
    match thread::Builder::new()
        .name(name)
        .spawn(move || watchdog(&sys, &stop2, interval, threshold))
    {
        Ok(_) => Some(stop),
        Err(e) => {
            log::error!("Cannot start arbiter watchdog: {e}");
            None
        }
    }
}

fn watchdog(sys: &Weak<SystemInner>, stop: &AtomicBool, interval: Duration, threshold: Duration) {
    loop {
        thread::sleep(interval);
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let Some(arbiters) = sys.upgrade().map(|sys| sys.arbiters.lock().list.clone()) else {
            return;
        };

        let now = Instant::now();
        for arb in arbiters {
            if !arb.is_running() {
                continue;
            }
            match arb.0.latency.check(now, threshold) {
                Stall::Started(age) => {
                    let task = arb.handle().current_task();
                    if let Some(task) = task {
                        log::error!(
                            "Arbiter {}({:?}) is stalled, no progress for {age:?}, running task spawned at {task}",
                            arb.name(),
                            arb.id()
                        );
                    } else {
                        log::error!(
                            "Arbiter {}({:?}) is stalled, no progress for {age:?}",
                            arb.name(),
                            arb.id()
                        );
                    }
                    #[cfg(target_os = "linux")]
                    capture_backtrace(&arb, age);
                }
                Stall::Recovered(time) => {
                    log::warn!(
                        "Arbiter {}({:?}) recovered after {time:?} stall",
                        arb.name(),
                        arb.id()
                    );
                }
                Stall::None => {}
            }
        }
    }
}

#[cfg(target_os = "linux")]
/// Sends `SIGUSR2` to the stalled arbiter thread to capture a backtrace.
fn capture_backtrace(arb: &Arbiter, age: Duration) {
    const SPIN: Duration = Duration::from_micros(100);

    if !crate::signals::is_enabled() {
        return;
    }
    let tid = arb.tid();
    if !CAPTURE.arm(tid) {
        return;
    }
    let result = unsafe { libc::syscall(libc::SYS_tgkill, libc::getpid(), tid, libc::SIGUSR2) };

    if result == -1 {
        log::error!(
            "Unable to send SIGUSR2 to arbiter {}({:?}): {}",
            arb.name(),
            arb.id(),
            std::io::Error::last_os_error()
        );
    } else {
        for _ in 0..1000 {
            thread::sleep(SPIN);
            if let Some(bt) = CAPTURE.take(tid) {
                let bt = ntex_error::Backtrace::from(bt);
                if let Some(f) = latency_callback() {
                    f(bt);
                } else {
                    bt.resolver().resolve();
                    log::error!(
                        "Arbiter {}({:?}) made no progress for {age:?}.\n{bt:?}",
                        arb.name(),
                        arb.id()
                    );
                }
                break;
            }
        }
    }
    CAPTURE.disarm(tid);
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
#[allow(clippy::type_complexity)]
static ARB_CB: Mutex<Option<Arc<dyn Fn(ntex_error::Backtrace) + Send + Sync>>> = Mutex::new(None);

#[cfg(target_os = "linux")]
#[allow(clippy::type_complexity)]
/// Clone the callback so it runs without the lock and survives replacement.
fn latency_callback() -> Option<Arc<dyn Fn(ntex_error::Backtrace) + Send + Sync>> {
    ARB_CB.lock().clone()
}

#[track_caller]
#[cfg(target_family = "unix")]
pub(crate) fn sig_usr2() {
    #[cfg(target_os = "linux")]
    #[allow(clippy::cast_possible_truncation)]
    {
        let tid = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
        CAPTURE.capture(tid, panic::Location::caller().file());
    }
}

#[cfg(test)]
mod ping_tests {
    use super::*;
    use crate::testing::TestRunner;

    #[test]
    fn ping_records_pruned() {
        System::build()
            .name("test")
            .ping_interval(5)
            .build(TestRunner)
            .block_on(async {
                let mut arb = Arbiter::new();
                let id = arb.id();
                let answered = || {
                    PINGS.with(|p| {
                        p.borrow()
                            .get(&id)
                            .is_some_and(|recs| recs.iter().any(|r| r.rtt.is_some()))
                    })
                };
                let has = || PINGS.with(|p| p.borrow().contains_key(&id));

                for _ in 0..400 {
                    if answered() {
                        break;
                    }
                    Delay::new(Duration::from_millis(5)).await;
                }
                assert!(answered());

                arb.stop();
                arb.join().unwrap();
                for _ in 0..400 {
                    if !has() {
                        break;
                    }
                    Delay::new(Duration::from_millis(5)).await;
                }
                assert!(!has());
            });
    }

    #[test]
    fn latency_sampling() {
        System::build()
            .name("test")
            .latency_interval(5)
            .build(TestRunner)
            .block_on(async {
                let mut arb = Arbiter::new();
                let sampled = |arb: &Arbiter| arb.latency().unwrap().histogram.count() >= 2;
                let sys_arb = System::current().arbiter();

                for _ in 0..400 {
                    if sampled(&arb) && sampled(&sys_arb) {
                        break;
                    }
                    Delay::new(Duration::from_millis(5)).await;
                }
                assert!(sampled(&arb));
                assert!(sampled(&sys_arb));

                let m = arb.latency().unwrap();
                assert_eq!(m.interval, Duration::from_millis(5));
                assert!(m.last_sample.unwrap() <= Instant::now());

                arb.stop();
                arb.join().unwrap();
            });

        System::build()
            .name("test")
            .latency_interval(0)
            .build(TestRunner)
            .block_on(async {
                assert_eq!(System::current().arbiter().latency(), None);
                let mut arb = Arbiter::new();
                assert_eq!(arb.latency(), None);
                arb.stop();
                arb.join().unwrap();
            });
    }

    #[test]
    fn stall_detection() {
        System::build()
            .name("test")
            .latency_interval(10)
            .ping_threshold(50)
            .build(TestRunner)
            .block_on(async {
                let mut arb = Arbiter::new();
                let (tx, rx) = oneshot::channel();
                let line = line!() + 1;
                arb.handle().spawn(async move {
                    std::thread::sleep(Duration::from_millis(300));
                    let _ = tx.send(());
                });

                let stalled = || arb.latency().unwrap().stalled;
                for _ in 0..400 {
                    if stalled() {
                        break;
                    }
                    Delay::new(Duration::from_millis(5)).await;
                }
                assert!(stalled());
                // the watchdog names the blocking task
                if let Some(task) = arb.handle().current_task() {
                    assert_eq!((task.file(), task.line()), (file!(), line));
                } else {
                    assert!(cfg!(any(feature = "tokio", feature = "compio")));
                }
                rx.await.unwrap();

                for _ in 0..400 {
                    if !stalled() {
                        break;
                    }
                    Delay::new(Duration::from_millis(5)).await;
                }
                let m = arb.latency().unwrap();
                assert!(!m.stalled);
                assert_eq!(m.stalls, 1);
                assert!(m.stall_duration >= Duration::from_millis(200), "{m:?}");

                let sys = System::current().arbiter().latency().unwrap();
                assert_eq!((sys.stalls, sys.stalled), (0, false));

                arb.stop();
                arb.join().unwrap();
            });
    }
}

#[cfg(test)]
mod api_tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::testing::TestRunner;

    #[test]
    #[should_panic(expected = "System is not running")]
    fn current_without_system() {
        assert!(System::try_current().is_none());
        let _ = System::current();
    }

    #[test]
    fn system_api() {
        let config = System::new("base", TestRunner).block_on(async {
            let sys = System::current();
            assert_eq!(sys.name(), "base");
            assert!(!sys.testing());
            assert!(!sys.signals());
            assert_eq!(System::try_current().unwrap().id(), sys.id());
            assert_eq!(sys.arbiter(), Arbiter::current());
            assert!(format!("{sys:?}").contains("base"));
            assert!(format!("{:?}", sys.config()).contains("base"));

            // values are stored once per system
            assert_eq!(sys.get_value(|| 1usize), 1);
            assert_eq!(sys.get_value(|| 2usize), 1);

            assert_eq!(sys.spawn_blocking(|| 10).await, Ok(10));

            let cnt = Arc::new(AtomicUsize::new(0));
            let cnt2 = cnt.clone();
            sys.handle()
                .spawn(async move {
                    cnt2.fetch_add(1, Ordering::Relaxed);
                })
                .await
                .unwrap();
            assert_eq!(cnt.load(Ordering::Relaxed), 1);
            sys.list_arbiters(|arbs| assert_eq!(arbs, [sys.arbiter()]));
            sys.config()
        });
        assert!(System::try_current().is_none());

        System::with_config("copy", config).block_on(async {
            let sys = System::current();
            assert_eq!(sys.name(), "copy");
            assert_eq!(Arbiter::current().name(), "copy");
        });
    }

    #[test]
    fn list_arbiter_pings() {
        System::build()
            .name("test")
            .ping_interval(5)
            .build(TestRunner)
            .block_on(async {
                let mut arb = Arbiter::new();
                let visited = || {
                    let mut found = false;
                    System::list_arbiter_pings(|a, recs| {
                        if a == &arb && recs.iter().any(|r| r.rtt.is_some()) {
                            found = true;
                        }
                    });
                    found
                };
                for _ in 0..400 {
                    if visited() {
                        break;
                    }
                    Delay::new(Duration::from_millis(5)).await;
                }
                assert!(visited());
                arb.stop();
                arb.join().unwrap();
            });
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::{panic::Location, sync::Arc};

    use super::*;

    #[test]
    fn latency_callback_replaced_while_running() {
        let calls = Arc::new(AtomicUsize::new(0));
        let data = Arc::new(vec![1u8; 64]);
        let calls2 = calls.clone();
        System::set_latency_callback(move |_| {
            // replace itself, then use captured state
            System::set_latency_callback(|_| {});
            assert_eq!(data.len(), 64);
            calls2.fetch_add(1, Ordering::Relaxed);
        });

        latency_callback().unwrap()(ntex_error::Backtrace::new(Location::caller()));
        latency_callback().unwrap()(ntex_error::Backtrace::new(Location::caller()));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        *ARB_CB.lock() = None;
    }
}
