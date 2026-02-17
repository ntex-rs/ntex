use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, atomic::AtomicUsize, atomic::Ordering};
use std::time::{Duration, Instant};
use std::{cell::RefCell, fmt, future::Future, panic, pin::Pin};

use async_channel::{Receiver, Sender};
use futures_timer::Delay;

use crate::pool::ThreadPool;
use crate::{Arbiter, BlockingResult, Builder, Runner, SystemRunner};

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

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Id(pub(crate) usize);

/// System is a runtime manager
#[derive(Clone, Debug)]
pub struct System {
    id: usize,
    sys: Sender<SystemCommand>,
    arbiter: Arbiter,
    config: SystemConfig,
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

impl System {
    /// Constructs new system and sets it as current
    pub(super) fn construct(
        sys: Sender<SystemCommand>,
        mut arbiter: Arbiter,
        config: SystemConfig,
        pool: ThreadPool,
    ) -> Self {
        let id = SYSTEM_COUNT.fetch_add(1, Ordering::SeqCst);
        arbiter.sys_id = id;

        let sys = System {
            id,
            sys,
            arbiter,
            config,
            pool,
        };
        System::set_current(sys.clone());
        sys
    }

    /// Build a new system with a customized tokio runtime
    ///
    /// This allows to customize the runtime. See struct level docs on
    /// `Builder` for more information.
    pub fn build() -> Builder {
        Builder::new()
    }

    #[allow(clippy::new_ret_no_self)]
    /// Create new system
    ///
    /// This method panics if it can not create tokio runtime
    pub fn new<R: Runner>(name: &str, runner: R) -> SystemRunner {
        Self::build().name(name).build(runner)
    }

    #[allow(clippy::new_ret_no_self)]
    /// Create new system
    ///
    /// This method panics if it can not create tokio runtime
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
        let _ = self.sys.try_send(SystemCommand::Exit(code));
    }

    /// Return status of `stop_on_panic` option
    ///
    /// It controls whether the System is stopped when an
    /// uncaught panic is thrown from a worker thread.
    pub fn stop_on_panic(&self) -> bool {
        self.config.stop_on_panic
    }

    /// System arbiter
    pub fn arbiter(&self) -> &Arbiter {
        &self.arbiter
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
        &self.sys
    }

    /// System config
    pub fn config(&self) -> SystemConfig {
        self.config.clone()
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
pub(super) struct SystemSupport {
    stop: Option<oneshot::Sender<i32>>,
    commands: Receiver<SystemCommand>,
    ping_interval: Duration,
}

impl SystemSupport {
    pub(super) fn new(
        stop: oneshot::Sender<i32>,
        commands: Receiver<SystemCommand>,
        ping_interval: usize,
    ) -> Self {
        Self {
            commands,
            stop: Some(stop),
            ping_interval: Duration::from_millis(ping_interval as u64),
        }
    }

    pub(super) async fn run(mut self) {
        ARBITERS.with(move |arbs| {
            let mut arbiters = arbs.borrow_mut();
            arbiters.all.clear();
            arbiters.list.clear();
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
