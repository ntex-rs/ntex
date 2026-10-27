use std::{fmt, future::Future, io, marker::PhantomData, panic, rc::Rc, sync::Arc, time};

use crate::{driver::Runner, signals, system::System, system::SystemConfig};

#[derive(Debug, Clone)]
/// Builder for an ntex runtime system.
///
/// Obtain a builder from [`System::build`]. Use [`build`](Self::build) to create
/// a [`SystemRunner`], then run the event loop or block on a future.
pub struct Builder {
    /// Name of the System. Defaults to "ntex" if unset.
    name: String,
    /// New thread stack size
    stack_size: usize,
    /// Arbiters ping interval
    ping_interval: usize,
    /// Arbiter ping response threshold
    ping_threshold: usize,
    /// Arbiter latency sampling interval
    latency_interval: usize,
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
            latency_interval: 100,
            signals: false,
            panics: false,
            testing: false,
            pool_limit: 256,
            pool_recv_timeout: time::Duration::from_mins(1),
        }
    }

    #[must_use]
    /// Sets the system name, used when naming its threads.
    ///
    /// The default is `"ntex"`.
    pub fn name<N: AsRef<str>>(mut self, name: N) -> Self {
        self.name = name.as_ref().into();
        self
    }

    #[must_use]
    /// Enables or disables process signal handling.
    ///
    /// By default, signal handling is disabled. At startup the system attempts
    /// to acquire process-wide ownership; another system's existing registration
    /// is not replaced. See [`System::enable_signals`].
    pub fn signals(mut self, eanbled: bool) -> Self {
        self.signals = eanbled;
        self
    }

    #[must_use]
    /// Enables or disables installation of the process-wide panic hook at startup.
    ///
    /// When panic handling is enabled, the application can receive
    /// `Signal::Panic(PanicSource::App(..))` signals while signal handling
    /// is enabled. The previously installed panic hook is still called.
    /// The hook is installed once and remains installed for the process lifetime;
    /// `false` does not remove a hook installed earlier. By default, installation
    /// is disabled.
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
    /// By default, signal handling is disabled.
    pub fn enable_signals(mut self) -> Self {
        self.signals = true;
        self
    }

    #[must_use]
    /// Sets the stack size, in bytes, for newly spawned arbiter threads.
    ///
    /// The default is zero, which uses the standard library's thread stack size.
    /// This does not change the primary arbiter's or blocking pool workers' stacks.
    pub fn stack_size(mut self, size: usize) -> Self {
        self.stack_size = size;
        self
    }

    #[must_use]
    /// Sets the ping interval for all registered arbiters, including the primary.
    ///
    /// The interval is specified in milliseconds and defaults to 2,000.
    /// Set it to zero to disable pings.
    pub fn ping_interval(mut self, interval: usize) -> Self {
        self.ping_interval = interval;
        self
    }

    #[must_use]
    /// Sets the arbiter stall threshold.
    ///
    /// A watchdog thread marks an arbiter as stalled when it records no
    /// latency sample for longer than the
    /// [`latency_interval`](Self::latency_interval) plus this threshold, see
    /// [`LatencyMetrics::stalls`](crate::metrics::LatencyMetrics::stalls). On Linux and
    /// Windows an attempt is made to create a backtrace for the stalled arbiter.
    ///
    /// The threshold is specified in milliseconds and defaults to 1,000.
    pub fn ping_threshold(mut self, interval: usize) -> Self {
        self.ping_threshold = interval;
        self
    }

    #[must_use]
    /// Sets the scheduling latency sampling interval for arbiters.
    ///
    /// Each arbiter runs a task that sleeps for the interval and records how
    /// late it wakes up, see [`Arbiter::latency`](crate::Arbiter::latency).
    ///
    /// The interval is specified in milliseconds and defaults to 100.
    /// Set it to zero to disable sampling and stall detection.
    pub fn latency_interval(mut self, interval: usize) -> Self {
        self.latency_interval = interval;
        self
    }

    #[must_use]
    /// Sets the maximum number of blocking thread-pool workers.
    ///
    /// The default is 256. Zero is treated as one; excess work is queued.
    pub fn thread_pool_limit(mut self, value: usize) -> Self {
        self.pool_limit = value;
        self
    }

    #[must_use]
    /// Configures the system for testing.
    ///
    /// Sets the testing flag and disables signal and panic handling on this
    /// builder. Subsequent calls can enable either again.
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
    /// This stores configuration only; the system starts when the returned
    /// [`SystemRunner`] is driven.
    pub fn build<R: Runner>(self, runner: R) -> SystemRunner {
        let config = SystemConfig {
            name: self.name.clone(),
            testing: self.testing,
            stack_size: self.stack_size,
            ping_interval: self.ping_interval,
            ping_threshold: self.ping_threshold,
            latency_interval: self.latency_interval,
            pool_limit: self.pool_limit,
            pool_recv_timeout: self.pool_recv_timeout,
            runner: Arc::new(runner),
        };
        self.build_with(config)
    }

    /// Creates a system runner from an existing system configuration.
    ///
    /// The supplied configuration determines the name, runtime runner, and all
    /// other system settings. Only this builder's signal and panic-handling
    /// settings are used. No event loop is started by this method.
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
///
/// Driving it installs a system and primary arbiter on the current thread.
/// This value is neither `Send` nor `Sync`.
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
    ///
    /// Equivalent to [`run`](Self::run) with an initialization callback that
    /// returns `Ok(())`, including its error and panic behavior.
    pub fn run_until_stop(self) -> io::Result<()> {
        self.run(|| Ok(()))
    }

    /// Runs `f`, then drives the event loop until [`System::stop()`] is called.
    ///
    /// The callback runs inside the event loop with the system and its primary
    /// arbiter installed. On exit, the system requests its arbiters to stop.
    ///
    /// # Errors
    ///
    /// Returns the initialization error from `f`, or an error if the system
    /// stops with a nonzero exit code or its stop channel closes unexpectedly.
    ///
    /// # Panics
    ///
    /// Resumes panics from initialization or the runtime runner after cleanup.
    /// Also panics if the runner returns successfully without completing its
    /// supplied future.
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
            let _guard = SystemGuard(system.clone());
            if signals {
                system.enable_signals();
            }

            f()?;

            let result = stop.await;

            match result {
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
    /// Runs the system until `fut` completes and returns its output.
    ///
    /// The future runs with the system and primary arbiter installed. Completion
    /// requests all system arbiters to stop. Unlike [`run`](Self::run), this
    /// waits for the future, not a stop request, and does not check the system's
    /// exit code.
    ///
    /// # Panics
    ///
    /// Resumes panics from the future or runtime runner after cleanup. Also
    /// panics if the runner returns without completing its supplied future.
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
            let _guard = SystemGuard(system.clone());
            if signals {
                system.enable_signals();
            }

            let loc = current_location();
            ntex_error::set_backtrace_start(loc.file(), loc.line() + 2);
            fut.await
        })
    }

    #[cfg(feature = "tokio")]
    /// Runs the system's future in a local task set on an existing Tokio runtime.
    ///
    /// Does not invoke the configured [`Runner`] or apply the builder's signal
    /// and panic-handling settings. The system and primary arbiter are installed
    /// while the future runs, and shutdown callbacks are run on normal completion.
    /// Like [`block_on`](Self::block_on), this waits for the future rather than
    /// a stop request.
    ///
    /// # Panics
    ///
    /// Panics if used without a compatible Tokio runtime context. Panics from
    /// the future propagate to the caller.
    pub async fn run_local<F, R>(self, fut: F) -> R
    where
        F: Future<Output = R> + 'static,
        R: 'static,
    {
        let SystemRunner { config, .. } = self;

        // run loop
        let result = tok_io::task::LocalSet::new()
            .run_until(async move {
                let (system, _) = System::start(config);
                let _guard = SystemGuard(system);

                let loc = current_location();
                ntex_error::set_backtrace_start(loc.file(), loc.line() + 2);
                fut.await
            })
            .await;

        crate::arbiter::run_shutdown_callbacks();
        unsafe {
            crate::remove_all_items();
        }
        if let Some(arb) = crate::Arbiter::try_current() {
            arb.finalize();
        }
        result
    }
}

/// Stops the system's arbiters and releases the signals for other systems when
/// the system's future completes, fails, panics or is dropped.
struct SystemGuard(System);

impl Drop for SystemGuard {
    fn drop(&mut self) {
        self.0.disable_signals();

        if std::thread::panicking() {
            // some executors (compio) abort the process if a task is woken
            // while the thread unwinds, stop arbiters from another thread
            let list = self.0.take_arbiters();
            if !list.is_empty() {
                let stopped = std::thread::Builder::new()
                    .spawn(move || list.into_iter().for_each(|arb| arb.stop()))
                    .map(std::thread::JoinHandle::join);
                if let Ok(Err(payload)) = stopped {
                    std::mem::forget(payload);
                    crate::abort_on_second_panic();
                }
            }
        } else {
            self.0.stop_arbiters();
        }
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

#[cfg(test)]
#[path = "../tests/unit/builder.rs"]
mod tests;
