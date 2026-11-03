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

pub use crate::{
    Affinity, ListenerMetrics, LoadBalancing, NoConfig, RuntimeMetrics, ServerAppConfig,
    ServerMetrics, ServerStatus, WorkerMetrics,
};

pub use self::accept::{AcceptLoop, AcceptNotify, AcceptorCommand};
pub use self::builder::{ServerBuilder, bind_addr, create_tcp_listener};
pub use self::config::{ServiceConfig, ServiceRuntime};
pub use self::service::StreamServer;
pub use self::socket::{Connection, Stream};
pub use self::test::{TestServer, TestServerBuilder, build_test_server, test_server};

/// Controller for a running network server.
pub type Server = crate::Server<Connection>;

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

#[cfg(test)]
mod tests {
    use super::*;

    /// STATEMENT: Network server diagnostics describe listener configuration and worker services.
    ///
    /// HOW:
    /// - Configure one named listener and check its token, backlog and socket name in diagnostics.
    /// - Build the stream server and worker service, checking the factory count and token routing.
    #[ntex::test]
    async fn debug_network_services() {
        use crate::ServerConfiguration;
        use ntex_service::cfg::SharedCfg;

        let config = ServiceConfig::<NoConfig>::new(Token(7), 64);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        config.listen("debug-listener", listener);
        config.on_worker_start(async |runtime| {
            runtime.service(
                "debug-listener",
                SharedCfg::default(),
                ntex_service::fn_service(async |_: ntex_io::Io| Ok::<_, ()>(())),
            );
            Ok(())
        });
        let debug = format!("{config:?}");
        assert!(debug.contains("ServiceConfigInner { token: Token(8), backlog: 64, sockets: ["));
        assert!(debug.contains("name: \"debug-listener\""));
        assert!(debug.contains("Token(7)"));

        let (_, _sockets, factory) = config.into_factory();
        let accept = AcceptLoop::new();
        let server = StreamServer::new(
            accept.notify(),
            std::sync::Arc::new(NoConfig),
            vec![factory],
        );
        assert_eq!(format!("{server:?}"), "StreamServer { services: 1 }");
        let service = server.create().await.unwrap();
        let debug = format!("{service:?}");
        assert!(debug.starts_with("StreamService { tokens: {Token(7): (0, "));
        assert!(debug.contains("conns: Counter("));
        assert!(debug.contains("count: Cell { value: 0 }"));
    }

    #[test]
    fn token_next() {
        let mut token = Token(0);
        assert_eq!(token.next(), Token(0));
        assert_eq!(token.next(), Token(1));
        assert_eq!(token, Token(2));
    }
}
