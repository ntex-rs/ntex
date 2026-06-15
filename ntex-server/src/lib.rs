#![deny(clippy::pedantic)]
#![allow(
    clippy::clone_on_copy,
    clippy::missing_fields_in_debug,
    clippy::must_use_candidate,
    clippy::missing_errors_doc
)]

use ntex_service::ServiceFactory;

mod manager;
pub mod net;
mod pool;
mod server;
mod wrk;

pub use self::pool::WorkerPool;
pub use self::server::Server;
pub use self::wrk::{Worker, WorkerStatus, WorkerStop};

#[deprecated(since = "3.10.0", note = "use ntex_rt::signals")]
pub use ntex_rt::signals::{Signal, signal};

/// Worker id
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WorkerId(pub(crate) usize);

impl WorkerId {
    pub(self) fn next(&mut self) -> WorkerId {
        let id = WorkerId(self.0);
        self.0 += 1;
        id
    }
}

#[allow(async_fn_in_trait)]
/// Worker service factory.
pub trait ServerConfiguration: Send + Clone + 'static {
    type Item: Send + 'static;
    type Factory: ServiceFactory<Self::Item> + 'static;

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
