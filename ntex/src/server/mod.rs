//! General purpose tcp server
use std::sync::atomic::{AtomicUsize, Ordering};

mod accept;
mod builder;
mod config;
mod counter;
mod service;
mod socket;
// mod test;

#[cfg(feature = "openssl")]
pub use ntex_tls::openssl;

#[cfg(feature = "rustls")]
pub use ntex_tls::rustls;

pub use ntex_tls::max_concurrent_ssl_accept;

pub(crate) use self::builder::create_tcp_listener;
pub use self::builder::ServerBuilder;
pub use self::config::{Config, ServiceConfig, ServiceRuntime};
// pub use self::test::{build_test_server, test_server, TestServer};
pub use self::socket::{Connection, Stream};

pub type Server = ntex_server::Server<Connection>;

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
struct Token(usize);

impl Token {
    pub(self) fn next(&mut self) -> Token {
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
    static MAX_CONNS_COUNTER: self::counter::Counter =
        self::counter::Counter::new(MAX_CONNS.load(Ordering::Relaxed));
}

/// Sets the maximum per-worker number of concurrent connections.
///
/// All socket listeners will stop accepting connections when this limit is
/// reached for each worker.
///
/// By default max connections is set to a 25k per worker.
pub(super) fn max_concurrent_connections(num: usize) {
    MAX_CONNS.store(num, Ordering::Relaxed);
}

pub(super) fn num_connections() -> usize {
    MAX_CONNS_COUNTER.with(|conns| conns.total())
}
