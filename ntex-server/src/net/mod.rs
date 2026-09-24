//! General-purpose network server.
//!
//! Use [`build()`] or [`ServerBuilder`] to register TCP or Unix domain socket
//! services. Each worker receives its own service instance and processes
//! connections on a single-threaded runtime.
use std::sync::atomic::{AtomicUsize, Ordering};

use ntex_util::services::Counter;

mod accept;
mod builder;
mod config;
mod factory;
mod service;
mod socket;
mod test;

pub use crate::{NoConfig, ServerAppConfig};

pub use self::accept::{AcceptLoop, AcceptNotify, AcceptorCommand};
pub use self::builder::{ServerBuilder, bind_addr, create_tcp_listener};
pub use self::config::{ServiceConfig, ServiceRuntime};
pub use self::service::StreamServer;
pub use self::socket::{Connection, Stream};
pub use self::test::{TestServer, TestServerBuilder, build_test_server, test_server};

/// Controller for a running network server.
pub type Server = crate::Server<Connection>;

#[non_exhaustive]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
/// Server readiness status.
pub enum ServerStatus {
    /// All workers are ready to accept work.
    Ready,
    /// At least one worker is temporarily unavailable.
    NotReady,
    /// A worker failed.
    WorkerFailed,
}

/// Identifier assigned to a registered listener.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Token(usize);

impl Token {
    #[must_use]
    #[allow(clippy::should_implement_trait)]
    /// Returns the current token and advances this value to the next token.
    pub fn next(&mut self) -> Token {
        let token = Token(self.0);
        self.0 += 1;
        token
    }
}

/// Creates a server builder with no application configuration.
pub fn build() -> ServerBuilder {
    ServerBuilder::default()
}

/// Creates a server builder with application configuration.
pub fn build_with_config<Cfg>(state: Cfg) -> ServerBuilder<Cfg>
where
    Cfg: ServerAppConfig,
{
    ServerBuilder::new(state)
}

static MAX_CONNS: AtomicUsize = AtomicUsize::new(25600);

thread_local! {
    static MAX_CONNS_COUNTER: Counter = Counter::new(MAX_CONNS.load(Ordering::Relaxed));
}

/// Sets the maximum per-worker number of concurrent connections.
///
/// By default, the limit is 25,600 connections per worker.
pub(super) fn max_concurrent_connections(num: usize) {
    MAX_CONNS.store(num, Ordering::Relaxed);
    MAX_CONNS_COUNTER.with(|conns| conns.set_capacity(num));
}

pub(super) fn num_connections() -> usize {
    MAX_CONNS_COUNTER.with(Counter::total)
}
