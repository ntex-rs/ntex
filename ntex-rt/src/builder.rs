use std::{fmt, future::Future, io, marker::PhantomData, panic, rc::Rc, sync::Arc, time};

use crate::{driver::Runner, signals, system::System, system::SystemConfig};

#[derive(Debug, Clone)]
/// Builder for an ntex runtime system.
///
/// Use [`build`](Self::build) to create a [`SystemRunner`], then run the event
/// loop or block on a future.
pub struct Builder {
    /// Name of the System. Defaults to "ntex" if unset.
    name: String,
    /// New thread stack size
    stack_size: usize,
    /// Arbiters ping interval
    ping_interval: usize,
    /// Arbiter ping response threshold
    ping_threshold: usize,
    /// Signal handling
    signals: bool,
    /// Panic handling
    panics: bool,
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
            stack_size: 0,
            ping_interval: 2000,
            ping_threshold: 1000,
            signals: false,
            panics: false,
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

    #[doc(hidden)]
    #[deprecated(since = "3.17.0")]
    #[must_use]
    /// Sets the option `stop_on_panic`
    ///
    /// It controls whether the System is stopped when an
    /// uncaught panic is thrown from a worker thread.
    ///
    /// Defaults is set to false.
    pub fn stop_on_panic(self, _: bool) -> Self {
        self
    }

    #[must_use]
    /// Enables or disables process signal handling.
    ///
    /// By default, signal handling is disabled.
    pub fn signals(mut self, eanbled: bool) -> Self {
        self.signals = eanbled;
        self
    }

    #[must_use]
    /// Enables panic handling.
    ///
    /// When panic handling is enabled, the application can receive
    /// `Signal::Panic(PanicReason::Panic(..))` signals.
    /// By default, panic handling is disabled.
    pub fn panic_handling(mut self, eanbled: bool) -> Self {
        self.panics = eanbled;
        self
    }

    #[doc(hidden)]
    #[must_use]
    /// Disable signal handling.
    ///
    /// By default, signal handling is disabled.
    pub fn disable_signals(mut self) -> Self {
        self.signals = false;
        self
    }

    #[doc(hidden)]
    #[must_use]
    /// Enable signal handling.
    ///
    /// By default, signal handling is enabled.
    pub fn enable_signals(mut self) -> Self {
        self.signals = true;
        self
    }

    #[must_use]
    /// Sets the size of the stack (in bytes) for the new worker thread.
    pub fn stack_size(mut self, size: usize) -> Self {
        self.stack_size = size;
        self
    }

    #[must_use]
    /// Sets the ping interval for spawned arbiters.
    ///
    /// The interval is specified in milliseconds and defaults to 2,000.
    /// Set it to zero to disable pings.
    pub fn ping_interval(mut self, interval: usize) -> Self {
        self.ping_interval = interval;
        self
    }

    #[must_use]
    /// Sets the ping response threshold.
    ///
    /// If a response takes too long, an attempt is made to create a backtrace
    /// for the busy arbiter.
    ///
    /// The threshold is specified in milliseconds and defaults to 1,000.
    pub fn ping_threshold(mut self, interval: usize) -> Self {
        self.ping_threshold = interval;
        self
    }

    #[must_use]
    /// Sets the maximum number of blocking thread-pool workers.
    ///
    /// The default is 256.
    pub fn thread_pool_limit(mut self, value: usize) -> Self {
        self.pool_limit = value;
        self
    }

    #[must_use]
    /// Configures the system for testing.
    ///
    /// This disables signal and panic handling.
    pub fn testing(mut self) -> Self {
        self.testing = true;
        self.signals = false;
        self.panics = false;
        self
    }

    #[must_use]
    /// Sets how long an idle blocking worker waits before exiting.
    ///
    /// The default is 60 seconds.
    pub fn thread_pool_recv_timeout<T>(mut self, timeout: T) -> Self
    where
        time::Duration: From<T>,
    {
        self.pool_recv_timeout = timeout.into();
        self
    }

    /// Creates a system runner using the specified runtime runner.
    ///
    /// # Panics
    ///
    /// Panics if the runtime cannot be created.
    pub fn build<R: Runner>(self, runner: R) -> SystemRunner {
        let config = SystemConfig {
            name: self.name.clone(),
            testing: self.testing,
            stack_size: self.stack_size,
            ping_interval: self.ping_interval,
            ping_threshold: self.ping_threshold,
            pool_limit: self.pool_limit,
            pool_recv_timeout: self.pool_recv_timeout,
            runner: Arc::new(runner),
        };
        self.build_with(config)
    }

    /// Creates a system runner from an existing system configuration.
    ///
    /// # Panics
    ///
    /// Panics if the runtime cannot be created.
    pub fn build_with(self, config: SystemConfig) -> SystemRunner {
        let runner = config.runner.clone();

        // init system arbiter and run configuration method
        SystemRunner {
            config,
            runner,
            signals: self.signals,
            panics: self.panics,
            _t: PhantomData,
        }
    }
}

/// A configured system that has not yet started its event loop.
#[must_use = "SystemRunner must be run"]
pub struct SystemRunner {
    config: SystemConfig,
    runner: Arc<dyn Runner>,
    signals: bool,
    panics: bool,
    _t: PhantomData<Rc<()>>,
}

impl SystemRunner {
    /// Runs the event loop until [`System::stop()`] is called.
    pub fn run_until_stop(self) -> io::Result<()> {
        self.run(|| Ok(()))
    }

    /// Runs `f`, then drives the event loop until [`System::stop()`] is called.
    pub fn run<F>(self, f: F) -> io::Result<()>
    where
        F: FnOnce() -> io::Result<()> + 'static,
    {
        log::info!("Starting {:?} system", self.config.name);

        let SystemRunner {
            config,
            runner,
            signals,
            panics,
            ..
        } = self;

        if panics {
            signals::enable_panic_handling();
        }

        // run loop
        crate::driver::block_on_panic(runner.as_ref(), async move {
            let (system, stop) = System::start(config);
            if signals {
                system.enable_signals();
            }

            f()?;

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

    #[allow(clippy::missing_panics_doc)]
    /// Execute a future and wait for result.
    pub fn block_on<F, R>(self, fut: F) -> R
    where
        F: Future<Output = R> + 'static,
        R: 'static,
    {
        let SystemRunner {
            config,
            runner,
            signals,
            panics,
            ..
        } = self;

        if panics {
            signals::enable_panic_handling();
        }

        crate::driver::block_on_panic(runner.as_ref(), async move {
            let (system, _) = System::start(config);
            if signals {
                system.enable_signals();
            }

            let loc = current_location();
            ntex_error::set_backtrace_start(loc.file(), loc.line() + 2);
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
        let result = tok_io::task::LocalSet::new()
            .run_until(async move {
                _ = System::start(config);

                let loc = current_location();
                ntex_error::set_backtrace_start(loc.file(), loc.line() + 2);
                fut.await
            })
            .await;

        crate::arbiter::run_shutdown_callbacks();
        unsafe {
            crate::remove_all_items();
        }
        result
    }
}

#[track_caller]
pub(crate) fn current_location() -> &'static panic::Location<'static> {
    panic::Location::caller()
}

impl fmt::Debug for SystemRunner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SystemRunner")
            .field("config", &self.config)
            .finish()
    }
}
