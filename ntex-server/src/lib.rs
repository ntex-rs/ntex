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

mod affinity;
mod balance;
mod manager;
mod metrics;
pub mod net;
mod pool;
mod server;
mod state;
mod status;
mod wrk;

pub use self::affinity::Affinity;
pub use self::balance::LoadBalancing;
pub use self::metrics::{ListenerMetrics, RuntimeMetrics, ServerMetrics, WorkerMetrics};
pub use self::pool::WorkerPool;
pub use self::server::Server;
pub use self::state::{NoConfig, ServerAppConfig};
pub use self::status::ServerStatus;
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

    /// Called when the server status changes from
    /// [`Ready`](ServerStatus::Ready) to paused or unavailable.
    ///
    /// The server is paused by [`Server::pause`], and it is unavailable
    /// while no worker can accept items. Not called when the server stops.
    fn pause(&self) {}

    /// Called when the server status becomes [`Ready`](ServerStatus::Ready).
    ///
    /// The server is ready while it is not paused and a worker is available.
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

    /// Returns the index of the logical CPU that receives the network packets
    /// of the item, if known.
    ///
    /// Used by [`LoadBalancing::IncomingCpu`] to send the item to the worker
    /// placed on that CPU. CPUs are numbered as in [`Worker::start`]. Network
    /// servers return the receive-side scaling (RSS) CPU of the connection on
    /// Windows, and its `SO_INCOMING_CPU` on Linux and Android. The default
    /// returns `None`.
    ///
    /// Called on the thread that submits the item with [`Server::process`],
    /// e.g. the accept thread of a network server.
    /// Calls are not concurrent. An item submitted while the method runs,
    /// e.g. by the method itself, is sent without its incoming CPU.
    fn incoming_cpu(&self, item: &Self::Item) -> Option<usize> {
        let _ = item;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_id_next() {
        let mut id = WorkerId::default();
        assert_eq!(id.next(), WorkerId(0));
        assert_eq!(id.next(), WorkerId(1));
        assert_eq!(id, WorkerId(2));
    }
}
