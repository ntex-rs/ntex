//! General purpose tcp server
use ntex_util::services::Counter;
use std::sync::atomic::{AtomicUsize, Ordering};

mod accept;
mod builder;
mod config;
mod factory;
mod service;
mod socket;
mod test;

pub use self::accept::{AcceptLoop, AcceptNotify, AcceptorCommand};
pub use self::builder::{bind_addr, create_tcp_listener, ServerBuilder};
pub use self::config::{Config, ServiceConfig, ServiceRuntime};
pub use self::service::StreamServer;
pub use self::socket::{Connection, Stream};
pub use self::test::{build_test_server, test_server, TestServer};

pub type Server = crate::Server<Connection>;

#[non_exhaustive]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
/// Server readiness status
pub enum ServerStatus {
    Ready,
    NotReady,
    WorkerFailed,
}

/// Socket id token
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Token(usize);

impl Token {
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Token {
        let token = Token(self.0);
        self.0 += 1;
        token
    }
}

/// Start server building process
pub fn build() -> ServerBuilder {
    ServerBuilder::default()
}

/// Ssl error combinded with service error.
#[derive(Debug)]
pub enum SslError<E> {
    Ssl(Box<dyn std::error::Error>),
    Service(E),
}

static MAX_CONNS: AtomicUsize = AtomicUsize::new(25600);

thread_local! {
    static MAX_CONNS_COUNTER: Counter = Counter::new(MAX_CONNS.load(Ordering::Relaxed));
}

/// Sets the maximum per-worker number of concurrent connections.
///
/// All socket listeners will stop accepting connections when this limit is
/// reached for each worker.
///
/// By default max connections is set to a 25k per worker.
pub(super) fn max_concurrent_connections(num: usize) {
    MAX_CONNS.store(num, Ordering::Relaxed);
    MAX_CONNS_COUNTER.with(|conns| conns.set_capacity(num));
}

pub(super) fn num_connections() -> usize {
    MAX_CONNS_COUNTER.with(|conns| conns.total())
}
