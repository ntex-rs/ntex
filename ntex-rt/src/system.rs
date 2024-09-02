use std::sync::{atomic::AtomicUsize, atomic::Ordering, Arc};
use std::{cell::RefCell, fmt, future::Future, pin::Pin, rc::Rc};

use async_channel::Sender;

use super::arbiter::{Arbiter, SystemCommand};
use super::builder::{Builder, SystemRunner};

static SYSTEM_COUNT: AtomicUsize = AtomicUsize::new(0);

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
        arbiter: Arbiter,
        config: SystemConfig,
    ) -> Self {
        let sys = System {
            sys,
            config,
            arbiter,
            id: SYSTEM_COUNT.fetch_add(1, Ordering::SeqCst),
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
