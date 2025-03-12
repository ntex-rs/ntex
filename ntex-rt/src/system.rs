use std::sync::{atomic::AtomicUsize, atomic::Ordering, Arc};
use std::{cell::RefCell, collections::HashMap, fmt, future::Future, pin::Pin, rc::Rc};

use async_channel::{Receiver, Sender};

use super::arbiter::Arbiter;
use super::builder::{Builder, SystemRunner};

static SYSTEM_COUNT: AtomicUsize = AtomicUsize::new(0);

thread_local!(
    static ARBITERS: RefCell<Arbiters> = RefCell::new(Arbiters::default());
);

#[derive(Default)]
struct Arbiters {
    all: HashMap<usize, Arbiter>,
    list: Vec<Arbiter>,
}

/// System is a runtime manager.
#[derive(Clone, Debug)]
pub struct System {
    id: usize,
    sys: Sender<SystemCommand>,
    arbiter: Arbiter,
    config: SystemConfig,
}

#[derive(Clone)]
pub(super) struct SystemConfig {
    pub(super) stack_size: usize,
    pub(super) stop_on_panic: bool,
    pub(super) block_on:
        Option<Arc<dyn Fn(Pin<Box<dyn Future<Output = ()>>>) + Sync + Send>>,
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
    ) -> Self {
        let id = SYSTEM_COUNT.fetch_add(1, Ordering::SeqCst);
        arbiter.sys_id = id;

        let sys = System {
            id,
            sys,
            config,
            arbiter,
        };
        System::set_current(sys.clone());
        sys
    }

    /// Build a new system with a customized tokio runtime.
    ///
    /// This allows to customize the runtime. See struct level docs on
    /// `Builder` for more information.
    pub fn build() -> Builder {
        Builder::new()
    }

    #[allow(clippy::new_ret_no_self)]
    /// Create new system.
    ///
    /// This method panics if it can not create tokio runtime
    pub fn new(name: &str) -> SystemRunner {
        Self::build().name(name).finish()
    }

    /// Get current running system.
    pub fn current() -> System {
        CURRENT.with(|cell| match *cell.borrow() {
            Some(ref sys) => sys.clone(),
            None => panic!("System is not running"),
        })
    }

    /// Set current running system.
    #[doc(hidden)]
    pub fn set_current(sys: System) {
        CURRENT.with(|s| {
            *s.borrow_mut() = Some(sys);
        })
    }

    /// System id
    pub fn id(&self) -> usize {
        self.id
    }

    /// Stop the system
    pub fn stop(&self) {
        self.stop_with_code(0)
    }

    /// Stop the system with a particular exit code.
    pub fn stop_with_code(&self, code: i32) {
        let _ = self.sys.try_send(SystemCommand::Exit(code));
    }

    /// Return status of 'stop_on_panic' option which controls whether the System is stopped when an
    /// uncaught panic is thrown from a worker thread.
    pub fn stop_on_panic(&self) -> bool {
        self.config.stop_on_panic
    }

    /// System arbiter
    pub fn arbiter(&self) -> &Arbiter {
        &self.arbiter
    }

    /// Retrieves a list of all arbiters in the system.
    ///
    /// This method should be called from the context where the system has been initialized
    pub fn list_arbiters<F, R>(f: F) -> R
    where
        F: FnOnce(&[Arbiter]) -> R,
    {
        ARBITERS.with(|arbs| f(arbs.borrow().list.as_ref()))
    }

    pub(super) fn sys(&self) -> &Sender<SystemCommand> {
        &self.sys
    }

    /// System config
    pub(super) fn config(&self) -> SystemConfig {
        self.config.clone()
    }
}

impl SystemConfig {
    /// Execute a future with custom `block_on` method and wait for result.
    #[inline]
    pub(super) fn block_on<F, R>(&self, fut: F) -> R
    where
        F: Future<Output = R> + 'static,
        R: 'static,
    {
        // run loop
        let result = Rc::new(RefCell::new(None));
        let result_inner = result.clone();

        if let Some(block_on) = &self.block_on {
            (*block_on)(Box::pin(async move {
                let r = fut.await;
                *result_inner.borrow_mut() = Some(r);
            }));
        } else {
            crate::block_on(Box::pin(async move {
                let r = fut.await;
                *result_inner.borrow_mut() = Some(r);
            }));
        }
        let res = result.borrow_mut().take().unwrap();
        res
    }
}

impl fmt::Debug for SystemConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SystemConfig")
            .field("stack_size", &self.stack_size)
            .field("stop_on_panic", &self.stop_on_panic)
            .finish()
    }
}

#[derive(Debug)]
pub(super) enum SystemCommand {
    Exit(i32),
    RegisterArbiter(usize, Arbiter),
    UnregisterArbiter(usize),
}

pub(super) struct SystemSupport {
    stop: Option<oneshot::Sender<i32>>,
    commands: Receiver<SystemCommand>,
    _ping_interval: usize,
}

impl SystemSupport {
    pub(super) fn new(
        stop: oneshot::Sender<i32>,
        commands: Receiver<SystemCommand>,
        _ping_interval: usize,
    ) -> Self {
        Self {
            commands,
            _ping_interval,
            stop: Some(stop),
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
                    log::debug!("Stopping system with {} code", code);

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
                Ok(SystemCommand::RegisterArbiter(name, hnd)) => {
                    ARBITERS.with(move |arbs| {
                        let mut arbiters = arbs.borrow_mut();
                        arbiters.all.insert(name, hnd.clone());
                        arbiters.list.push(hnd);
                    })
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

pub(super) trait FnExec: Send + 'static {
    fn call_box(self: Box<Self>);
}

impl<F> FnExec for F
where
    F: FnOnce() + Send + 'static,
{
    #[allow(clippy::boxed_local)]
    fn call_box(self: Box<Self>) {
        (*self)()
    }
}
