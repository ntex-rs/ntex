use ntex_util::time::Millis;

use crate::{Server, ServerConfiguration, manager::ServerManager};

const DEFAULT_SHUTDOWN_TIMEOUT: Millis = Millis::from_secs(30);

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone)]
/// Builder for a pool of server workers.
pub struct WorkerPool {
    pub(crate) num: usize,
    pub(crate) name: String,
    pub(crate) no_signals: bool,
    pub(crate) stop_runtime: bool,
    pub(crate) stop_on_panic: bool,
    pub(crate) graceful_shutdown: bool,
    pub(crate) graceful_shutdown_timeout: Millis,
    pub(crate) affinity: bool,
}

impl Default for WorkerPool {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkerPool {
    #[must_use]
    /// Creates a worker pool with default settings.
    pub fn new() -> Self {
        let num = core_affinity::get_core_ids().map_or_else(
            || std::thread::available_parallelism().map_or(2, std::num::NonZeroUsize::get),
            |v| v.len(),
        );

        WorkerPool {
            num,
            name: "ntex".to_string(),
            no_signals: false,
            stop_runtime: false,
            stop_on_panic: false,
            graceful_shutdown: false,
            graceful_shutdown_timeout: DEFAULT_SHUTDOWN_TIMEOUT,
            affinity: false,
        }
    }

    #[must_use]
    /// Sets the worker thread name prefix.
    ///
    /// The configured name is used for worker thread names.
    pub fn name<T: AsRef<str>>(mut self, name: T) -> Self {
        self.name = name.as_ref().to_string();
        self
    }

    #[must_use]
    /// Sets the number of worker threads to start.
    ///
    /// By default, the server uses the number of available logical CPUs.
    pub fn workers(mut self, num: usize) -> Self {
        self.num = num;
        self
    }

    #[must_use]
    /// Stops the current ntex runtime when the server manager is dropped.
    ///
    /// By default "stop runtime" is disabled.
    pub fn stop_runtime(mut self) -> Self {
        self.stop_runtime = true;
        self
    }

    #[must_use]
    /// Stops the server when one of the workers panics.
    ///
    /// By default, "stop on panic" is disabled.
    pub fn stop_on_panic(mut self) -> Self {
        self.stop_on_panic = true;
        self
    }

    #[must_use]
    /// Disable signal handling.
    ///
    /// By default, signal handling is enabled.
    pub fn disable_signals(mut self) -> Self {
        self.no_signals = true;
        self
    }

    #[must_use]
    /// Graceful shutdown.
    ///
    /// Gracefully shuts down on SIGSEGV or SIGQUIT and app panics.
    /// Graceful shutdown is always enabled for SIGTERM.
    /// By default, it is disabled for SIGSEGV and SIGQUIT and panics.
    pub fn graceful_shutdown(mut self) -> Self {
        self.graceful_shutdown = true;
        self
    }

    #[must_use]
    /// Timeout for graceful worker shutdown.
    ///
    /// After receiving a stop signal, workers have this much time to finish
    /// serving requests. Workers that are still alive after the timeout are
    /// forcefully dropped.
    ///
    /// This bounds the worker as a whole, not an individual connection. Each
    /// connection is bound separately by `IoConfig::set_shutdown_timeout`, so
    /// this value should leave room for the connections a worker is still
    /// draining to shut down themselves.
    ///
    /// By default, the timeout is set to 30 seconds.
    pub fn graceful_shutdown_timeout<T: Into<Millis>>(mut self, timeout: T) -> Self {
        self.graceful_shutdown_timeout = timeout.into();
        self
    }

    #[must_use]
    /// Enables CPU affinity for worker threads.
    ///
    /// By default, affinity is disabled.
    pub fn enable_affinity(mut self) -> Self {
        self.affinity = true;
        self
    }

    /// Starts processing incoming items and returns a server controller.
    pub fn run<F: ServerConfiguration>(self, factory: F) -> Server<F::Item> {
        ServerManager::start(self, factory)
    }
}
