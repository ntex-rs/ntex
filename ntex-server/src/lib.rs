//! Worker-based server infrastructure for ntex.
//!
//! [`WorkerPool`] runs services across one or more worker threads. The [`net`]
//! module builds TCP and Unix domain socket servers on top of that pool.
//! [`Server`] is the controller used to pause, resume, stop, or await a running
//! server.

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
mod state;
mod wrk;

pub use self::pool::WorkerPool;
pub use self::server::Server;
pub use self::state::{NoConfig, ServerAppConfig};
pub use self::wrk::{Worker, WorkerStatus, WorkerStop};

/// Identifier assigned to a server worker.
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
    /// Item dispatched to a worker service.
    type Item: Send + 'static;
    /// Service created independently for each worker.
    type Service: Service<(), Self::Item, Res = (), Error = ()> + 'static;

    /// Creates the service used by one worker.
    async fn create(&self) -> std::io::Result<Self::Service>;

    /// Called when the server is paused.
    ///
    /// Besides explicit [`Server::pause`] calls, the server pauses itself
    /// while no worker is available to accept items.
    fn pause(&self) {}

    /// Called when the server is resumed.
    ///
    /// Besides explicit [`Server::resume`] calls, the server resumes itself
    /// once a worker becomes available again.
    fn resume(&self) {}

    /// Called when the server's command loop exits.
    ///
    /// This happens after a stop requested through [`Server::stop`] or a
    /// signal, graceful or not, once [`stop`](Self::stop) has completed.
    fn terminate(&self) {}

    /// Performs asynchronous cleanup when the server stops.
    ///
    /// Called once, before the workers are stopped.
    async fn stop(&self) {}
}
