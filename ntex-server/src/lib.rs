#![deny(rust_2018_idioms, unreachable_pub)]
#![allow(clippy::let_underscore_future)]

use ntex_service::ServiceFactory;
use ntex_util::time::Millis;

mod manager;
pub mod net;
mod pool;
mod server;
mod signals;
mod wrk;

pub use self::pool::WorkerPool;
pub use self::server::Server;
pub use self::wrk::{Worker, WorkerStatus, WorkerStop};

#[doc(hidden)]
pub use self::signals::{signal, Signal};

/// Worker id
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WorkerId(usize);

impl WorkerId {
    pub(self) fn next(&mut self) -> WorkerId {
        let id = WorkerId(self.0);
        self.0 += 1;
        id
    }
}

#[non_exhaustive]
#[derive(Debug)]
/// Worker message
pub enum WorkerMessage<T> {
    /// New item received
    New(T),
    /// Graceful shutdown in millis
    Shutdown(Millis),
    /// Force shutdown
    ForceShutdown,
}

#[allow(async_fn_in_trait)]
/// Worker service factory.
pub trait ServerConfiguration: Send + Clone + 'static {
    type Item: Send + 'static;
    type Factory: ServiceFactory<WorkerMessage<Self::Item>> + 'static;

    /// Create service factory for handling `WorkerMessage<T>` messages.
    async fn create(&self) -> Result<Self::Factory, ()>;

    /// Server is paused.
    fn paused(&self) {}

    /// Server is resumed.
    fn resumed(&self) {}

    /// Server is stopped
    fn terminate(&self) {}

    /// Server is stopped
    async fn stop(&self) {}
}
