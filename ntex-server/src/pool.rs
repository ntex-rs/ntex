use ntex_util::time::Millis;

use crate::{Server, ServerConfiguration};

const DEFAULT_SHUTDOWN_TIMEOUT: Millis = Millis::from_secs(30);

#[derive(Debug, Copy, Clone)]
/// Server builder
pub struct WorkerPool {
    pub(crate) num: usize,
    pub(crate) no_signals: bool,
    pub(crate) stop_runtime: bool,
    pub(crate) shutdown_timeout: Millis,
    pub(crate) affinity: bool,
}

impl Default for WorkerPool {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkerPool {
    /// Create new Server builder instance
    pub fn new() -> Self {
        WorkerPool {
            num: std::thread::available_parallelism()
                .map_or(2, std::num::NonZeroUsize::get),
            no_signals: false,
            stop_runtime: false,
            shutdown_timeout: DEFAULT_SHUTDOWN_TIMEOUT,
            affinity: false,
        }
    }

    /// Set number of workers to start.
    ///
    /// By default server uses number of available logical cpu as workers
    /// count.
    pub fn workers(mut self, num: usize) -> Self {
        self.num = num;
        self
    }

    /// Stop current ntex runtime when manager get dropped.
    ///
    /// By default "stop runtime" is disabled.
    pub fn stop_runtime(mut self) -> Self {
        self.stop_runtime = true;
        self
    }

    /// Disable signal handling.
    ///
    /// By default signal handling is enabled.
    pub fn disable_signals(mut self) -> Self {
        self.no_signals = true;
        self
    }

    /// Timeout for graceful workers shutdown.
    ///
    /// After receiving a stop signal, workers have this much time to finish
    /// serving requests. Workers still alive after the timeout are force
    /// dropped.
    ///
    /// By default shutdown timeout sets to 30 seconds.
    pub fn shutdown_timeout<T: Into<Millis>>(mut self, timeout: T) -> Self {
        self.shutdown_timeout = timeout.into();
        self
    }

    /// Enable core affinity
    ///
    /// By default affinity is disabled.
    pub fn enable_affinity(mut self) -> Self {
        self.affinity = true;
        self
    }

    /// Starts processing incoming items and return server controller.
    pub fn run<F: ServerConfiguration>(self, factory: F) -> Server<F::Item> {
        crate::manager::ServerManager::start(self, factory)
    }
}
