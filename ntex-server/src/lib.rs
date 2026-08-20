#![deny(clippy::pedantic)]
#![allow(
    async_fn_in_trait,
    clippy::clone_on_copy,
    clippy::must_use_candidate,
    clippy::missing_fields_in_debug,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::unused_async
)]

use ntex_service::Service;

mod manager;
pub mod net;
mod pool;
mod server;
mod wrk;

pub use self::pool::WorkerPool;
pub use self::server::Server;
pub use self::wrk::{Worker, WorkerStatus, WorkerStop};

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

/// Worker service configuration.
pub trait ServerConfiguration: Send + Clone + 'static {
    type Item: Send + 'static;
    type Service: Service<(), Self::Item, Res = (), Error = ()> + 'static;

    /// Create service for handling `WorkerMessage<T>` messages.
    async fn create(&self) -> Result<Self::Service, &'static str>;

    /// Pause the server.
    fn pause(&self) {}

    /// Resume the server.
    fn resume(&self) {}

    /// Server is stopped.
    fn terminate(&self) {}

    /// Server is stopped.
    async fn stop(&self) {}
}
