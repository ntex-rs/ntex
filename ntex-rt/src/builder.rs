use std::{fmt, future::Future, io, marker::PhantomData, rc::Rc, sync::Arc, time};

use async_channel::unbounded;

use crate::arbiter::{Arbiter, ArbiterController};
use crate::driver::Runner;
use crate::pool::ThreadPool;
use crate::system::{System, SystemCommand, SystemConfig, SystemSupport};

#[derive(Debug, Clone)]
/// Builder struct for a ntex runtime.
///
/// Either use `Builder::build` to create a system and start actors.
/// Alternatively, use `Builder::run` to start the tokio runtime and
/// run a function in its context.
pub struct Builder {
    /// Name of the System. Defaults to "ntex" if unset.
    name: String,
    /// Whether the Arbiter will stop the whole System on uncaught panic. Defaults to false.
    stop_on_panic: bool,
    /// New thread stack size
    stack_size: usize,
    /// Arbiters ping interval
    ping_interval: usize,
    /// Thread pool config
    pool_limit: usize,
    pool_recv_timeout: time::Duration,
    /// testing flag
    testing: bool,
}

impl Builder {
    pub(super) fn new() -> Self {
        Builder {
            name: "ntex".into(),
            stop_on_panic: false,
            stack_size: 0,
            ping_interval: 1000,
            testing: false,
            pool_limit: 256,
            pool_recv_timeout: time::Duration::from_secs(60),
        }
    }

    #[must_use]
    /// Sets the name of the System.
    pub fn name<N: AsRef<str>>(mut self, name: N) -> Self {
        self.name = name.as_ref().into();
        self
    }

    #[must_use]
    /// Sets the option `stop_on_panic`
    ///
    /// It controls whether the System is stopped when an
    /// uncaught panic is thrown from a worker thread.
    ///
    /// Defaults is set to false.
    pub fn stop_on_panic(mut self, stop_on_panic: bool) -> Self {
        self.stop_on_panic = stop_on_panic;
        self
    }

    #[must_use]
    /// Sets the size of the stack (in bytes) for the new thread.
    pub fn stack_size(mut self, size: usize) -> Self {
        self.stack_size = size;
        self
    }

    #[must_use]
    /// Sets ping interval for spawned arbiters.
    ///
    /// Interval is in milliseconds. By default 5000 milliseconds is set.
    /// To disable pings set value to zero.
    pub fn ping_interval(mut self, interval: usize) -> Self {
        self.ping_interval = interval;
        self
    }

    #[must_use]
    /// Set the thread number limit of the inner thread pool, if exists. The
    /// default value is 256.
    pub fn thread_pool_limit(mut self, value: usize) -> Self {
        self.pool_limit = value;
        self
    }

    #[must_use]
    /// Mark system as testing
    pub fn testing(mut self) -> Self {
        self.testing = true;
        self
    }

    #[must_use]
    /// Set the waiting timeout of the inner thread, if exists. The default is
    /// 60 seconds.
    pub fn thread_pool_recv_timeout<T>(mut self, timeout: T) -> Self
    where
        time::Duration: From<T>,
    {
        self.pool_recv_timeout = timeout.into();
        self
    }

    /// Create new System.
    ///
    /// This method panics if it can not create tokio runtime
    pub fn build<R: Runner>(self, runner: R) -> SystemRunner {
        let config = SystemConfig {
            name: self.name.clone(),
            testing: self.testing,
            stack_size: self.stack_size,
            stop_on_panic: self.stop_on_panic,
            ping_interval: self.ping_interval,
            pool_limit: self.pool_limit,
            pool_recv_timeout: self.pool_recv_timeout,
            runner: Arc::new(runner),
        };
        self.build_with(config)
    }

    /// Create new System.
    ///
    /// This method panics if it can not create tokio runtime
    pub fn build_with(self, config: SystemConfig) -> SystemRunner {
        // init system arbiter and run configuration method
        SystemRunner {
            runner: config.runner.clone(),
            config,
            _t: PhantomData,
        }
    }
}

/// Helper object that runs System's event loop
#[must_use = "SystemRunner must be run"]
pub struct SystemRunner {
    config: SystemConfig,
    runner: Arc<dyn Runner>,
    _t: PhantomData<Rc<()>>,
}

impl SystemRunner {
    /// This function will start event loop and will finish once the
    /// `System::stop()` function is called.
    pub fn run_until_stop(self) -> io::Result<()> {
        self.run(|| Ok(()))
    }

    /// This function will start event loop and will finish once the
    /// `System::stop()` function is called.
    pub fn run<F>(self, f: F) -> io::Result<()>
    where
        F: FnOnce() -> io::Result<()> + 'static,
    {
        log::info!("Starting {:?} system", self.config.name);

        let SystemRunner { config, runner, .. } = self;

        // run loop
        crate::driver::block_on(runner.as_ref(), async move {
            let (_, support, controller, stop) = create(&config);

            f()?;

            crate::spawn(support.run());
            crate::spawn(controller.run());
            match stop.await {
                Ok(code) => {
                    if code != 0 {
                        Err(io::Error::other(format!("Non-zero exit code: {code}")))
                    } else {
                        Ok(())
                    }
                }
                Err(_) => Err(io::Error::other("Closed")),
            }
        })
    }

    /// Execute a future and wait for result.
    pub fn block_on<F, R>(self, fut: F) -> R
    where
        F: Future<Output = R> + 'static,
        R: 'static,
    {
        let SystemRunner { config, runner, .. } = self;

        crate::driver::block_on(runner.as_ref(), async move {
            let (_, support, controller, _) = create(&config);

            crate::spawn(support.run());
            crate::spawn(controller.run());
            fut.await
        })
    }

    #[cfg(feature = "tokio")]
    /// Execute a future and wait for result.
    pub async fn run_local<F, R>(self, fut: F) -> R
    where
        F: Future<Output = R> + 'static,
        R: 'static,
    {
        let SystemRunner { config, .. } = self;

        // run loop
        tok_io::task::LocalSet::new()
            .run_until(async move {
                let (sys, support, controller, _) = create(&config);

                crate::spawn(support.run());
                crate::spawn(controller.run());
                fut.await
            })
            .await
    }
}

impl fmt::Debug for SystemRunner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SystemRunner")
            .field("config", &self.config)
            .finish()
    }
}

/// Create new System.
///
/// This method panics if it can not create tokio runtime
fn create(
    config: &SystemConfig,
) -> (
    System,
    SystemSupport,
    ArbiterController,
    oneshot::Receiver<i32>,
) {
    let (stop_tx, stop) = oneshot::channel();
    let (sys_sender, sys_receiver) = unbounded();

    // thread pool
    let pool = ThreadPool::new(&config.name, config.pool_limit, config.pool_recv_timeout);

    let (arb, controller) = Arbiter::new_system(config.name.clone());
    let _ = sys_sender.try_send(SystemCommand::RegisterArbiter(arb.id(), arb.clone()));
    let system = System::construct(sys_sender, arb, config.clone(), pool);

    // system arbiter
    let support = SystemSupport::new(stop_tx, sys_receiver, config.ping_interval);

    (system, support, controller, stop)
}
