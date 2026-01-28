use std::{fmt, io, net, sync::Arc};

use socket2::{Domain, SockAddr, Socket, Type};

use ntex_io::Io;
use ntex_rt::System;
use ntex_service::{ServiceFactory, cfg::SharedCfg};
use ntex_util::time::Millis;

use crate::{Server, WorkerPool};

use super::accept::AcceptLoop;
use super::config::{Config, ServiceConfig};
use super::factory::{self, FactoryServiceType};
use super::factory::{OnAccept, OnAcceptWrapper, OnWorkerStart, OnWorkerStartWrapper};
use super::{Connection, ServerStatus, Stream, StreamServer, Token, socket::Listener};

/// Streaming service builder
///
/// This type can be used to construct an instance of `net streaming server` through a
/// builder-like pattern.
pub struct ServerBuilder {
    name: String,
    token: Token,
    backlog: i32,
    services: Vec<FactoryServiceType>,
    sockets: Vec<(Token, String, Listener)>,
    on_worker_start: Vec<Box<dyn OnWorkerStart + Send>>,
    on_accept: Option<Box<dyn OnAccept + Send>>,
    accept: AcceptLoop,
    pool: WorkerPool,
}

impl Default for ServerBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for ServerBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerBuilder")
            .field("name", &self.name)
            .field("token", &self.token)
            .field("backlog", &self.backlog)
            .field("sockets", &self.sockets)
            .field("accept", &self.accept)
            .field("worker-pool", &self.pool)
            .finish()
    }
}

impl ServerBuilder {
    /// Create new Server builder instance
    pub fn new() -> ServerBuilder {
        let sys = System::current();
        let mut accept = AcceptLoop::default();
        if sys.testing() {
            accept.testing()
        }

        ServerBuilder {
            accept,
            name: sys.name().to_string(),
            token: Token(0),
            services: Vec::new(),
            sockets: Vec::new(),
            on_accept: None,
            on_worker_start: Vec::new(),
            backlog: 2048,
            pool: WorkerPool::default(),
        }
    }

    /// Set server name.
    ///
    /// Name is used for worker thread name
    pub fn name<T: AsRef<str>>(mut self, name: T) -> Self {
        self.name = name.as_ref().to_string();
        self.accept.name(self.name.as_str());
        self.pool = self.pool.name(self.name.as_str());
        self
    }

    /// Set number of workers to start.
    ///
    /// By default server uses number of available logical cpu as workers
    /// count.
    pub fn workers(mut self, num: usize) -> Self {
        self.pool = self.pool.workers(num);
        self
    }

    /// Set the maximum number of pending connections.
    ///
    /// This refers to the number of clients that can be waiting to be served.
    /// Exceeding this number results in the client getting an error when
    /// attempting to connect. It should only affect servers under significant
    /// load.
    ///
    /// Generally set in the 64-2048 range. Default value is 2048.
    ///
    /// This method should be called before `bind()` method call.
    pub fn backlog(mut self, num: i32) -> Self {
        self.backlog = num;
        self
    }

    /// Sets the maximum per-worker number of concurrent connections.
    ///
    /// All socket listeners will stop accepting connections when this limit is
    /// reached for each worker.
    ///
    /// By default max connections is set to a 25k per worker.
    pub fn maxconn(self, num: usize) -> Self {
        super::max_concurrent_connections(num);
        self
    }

    /// Stop ntex runtime when server get dropped.
    ///
    /// By default "stop runtime" is disabled.
    pub fn stop_runtime(mut self) -> Self {
        self.pool = self.pool.stop_runtime();
        self
    }

    /// Disable signal handling.
    ///
    /// By default signal handling is enabled.
    pub fn disable_signals(mut self) -> Self {
        self.pool = self.pool.disable_signals();
        self
    }

    /// Enable cpu affinity
    ///
    /// By default affinity is disabled.
    pub fn enable_affinity(mut self) -> Self {
        self.pool = self.pool.enable_affinity();
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
        self.pool = self.pool.shutdown_timeout(timeout);
        self
    }

    /// Set server status handler.
    ///
    /// Server calls this handler on every inner status update.
    pub fn status_handler<F>(mut self, handler: F) -> Self
    where
        F: FnMut(ServerStatus) + Send + 'static,
    {
        self.accept.set_status_handler(handler);
        self
    }

    /// Execute external async configuration as part of the server building
    /// process.
    ///
    /// This function is useful for moving parts of configuration to a
    /// different module or even library.
    pub async fn configure<F>(mut self, f: F) -> io::Result<ServerBuilder>
    where
        F: AsyncFn(ServiceConfig) -> io::Result<()>,
    {
        let cfg = ServiceConfig::new(self.token, self.backlog);

        f(cfg.clone()).await?;

        let (token, sockets, factory) = cfg.into_factory();
        self.token = token;
        self.sockets.extend(sockets);
        self.services.push(factory);

        Ok(self)
    }

    /// Register async service configuration function.
    ///
    /// This function get called during worker runtime configuration stage.
    /// It get executed in the worker thread.
    pub fn on_worker_start<F, E>(mut self, f: F) -> Self
    where
        F: AsyncFn() -> Result<(), E> + Send + Clone + 'static,
        E: fmt::Display + 'static,
    {
        self.on_worker_start.push(OnWorkerStartWrapper::create(f));
        self
    }

    /// Register on-accept callback function.
    ///
    /// This function get called with accepted stream.
    pub fn on_accept<F, E>(mut self, f: F) -> Self
    where
        F: AsyncFn(Arc<str>, Stream) -> Result<Stream, E> + Send + Clone + 'static,
        E: fmt::Display + 'static,
    {
        self.on_accept = Some(OnAcceptWrapper::create(f));
        self
    }

    /// Add new service to the server.
    pub fn bind<F, U, N, R>(mut self, name: N, addr: U, factory: F) -> io::Result<Self>
    where
        U: net::ToSocketAddrs,
        N: AsRef<str>,
        F: AsyncFn(Config) -> R + Send + Clone + 'static,
        R: ServiceFactory<Io, SharedCfg> + 'static,
    {
        let sockets = bind_addr(addr, self.backlog)?;

        let mut tokens = Vec::new();
        for lst in sockets {
            let token = self.token.next();
            self.sockets
                .push((token, name.as_ref().to_string(), Listener::from_tcp(lst)));
            tokens.push((token, SharedCfg::default()));
        }

        self.services.push(factory::create_factory_service(
            name.as_ref().to_string(),
            tokens,
            factory,
        ));

        Ok(self)
    }

    #[cfg(unix)]
    /// Add new unix domain service to the server.
    pub fn bind_uds<F, U, N, R>(self, name: N, addr: U, factory: F) -> io::Result<Self>
    where
        N: AsRef<str>,
        U: AsRef<std::path::Path>,
        F: AsyncFn(Config) -> R + Send + Clone + 'static,
        R: ServiceFactory<Io, SharedCfg> + 'static,
    {
        use std::os::unix::net::UnixListener;

        // The path must not exist when we try to bind.
        // Try to remove it to avoid bind error.
        if let Err(e) = std::fs::remove_file(addr.as_ref()) {
            // NotFound is expected and not an issue. Anything else is.
            if e.kind() != std::io::ErrorKind::NotFound {
                return Err(e);
            }
        }

        let lst = UnixListener::bind(addr)?;
        self.listen_uds(name, lst, factory)
    }

    #[cfg(unix)]
    /// Add new unix domain service to the server.
    /// Useful when running as a systemd service and
    /// a socket FD can be acquired using the systemd crate.
    pub fn listen_uds<F, N: AsRef<str>, R>(
        mut self,
        name: N,
        lst: std::os::unix::net::UnixListener,
        factory: F,
    ) -> io::Result<Self>
    where
        F: AsyncFn(Config) -> R + Send + Clone + 'static,
        R: ServiceFactory<Io, SharedCfg> + 'static,
    {
        let token = self.token.next();
        self.services.push(factory::create_factory_service(
            name.as_ref().to_string(),
            vec![(token, SharedCfg::default())],
            factory,
        ));
        self.sockets
            .push((token, name.as_ref().to_string(), Listener::from_uds(lst)));
        Ok(self)
    }

    /// Add new service to the server.
    pub fn listen<F, N: AsRef<str>, R>(
        mut self,
        name: N,
        lst: net::TcpListener,
        factory: F,
    ) -> io::Result<Self>
    where
        F: AsyncFn(Config) -> R + Send + Clone + 'static,
        R: ServiceFactory<Io, SharedCfg> + 'static,
    {
        let token = self.token.next();
        self.services.push(factory::create_factory_service(
            name.as_ref().to_string(),
            vec![(token, SharedCfg::default())],
            factory,
        ));
        self.sockets
            .push((token, name.as_ref().to_string(), Listener::from_tcp(lst)));
        Ok(self)
    }

    /// Set shared config for named service.
    pub fn config<N, U>(mut self, name: N, cfg: U) -> Self
    where
        N: AsRef<str>,
        U: Into<SharedCfg>,
    {
        let cfg = cfg.into();
        let mut token = None;
        for sock in &self.sockets {
            if sock.1 == name.as_ref() {
                token = Some(sock.0);
                break;
            }
        }

        if let Some(token) = token {
            for svc in &mut self.services {
                if svc.name(token) == name.as_ref() {
                    svc.set_config(token, cfg);
                }
            }
        } else {
            panic!("Cannot find service by name {:?}", name.as_ref());
        }

        self
    }

    #[doc(hidden)]
    /// Mark server as testing
    pub fn testing(mut self) -> Self {
        self.accept.testing();
        self
    }

    /// Starts processing incoming connections and return server controller.
    pub fn run(self) -> Server<Connection> {
        if self.sockets.is_empty() {
            panic!("Server should have at least one bound socket");
        } else {
            let srv = StreamServer::new(
                self.accept.notify(),
                self.services,
                self.on_worker_start,
                self.on_accept,
            );
            let svc = self.pool.run(srv);

            let sockets = self
                .sockets
                .into_iter()
                .map(|sock| {
                    log::info!("Starting \"{}\" service on {}", sock.1, sock.2);
                    (sock.0, sock.2)
                })
                .collect();
            self.accept.start(sockets, svc.clone());

            svc
        }
    }
}

pub fn bind_addr<S: net::ToSocketAddrs>(
    addr: S,
    backlog: i32,
) -> io::Result<Vec<net::TcpListener>> {
    let mut err = None;
    let mut succ = false;
    let mut sockets = Vec::new();
    for addr in addr.to_socket_addrs()? {
        match create_tcp_listener(addr, backlog) {
            Ok(lst) => {
                succ = true;
                sockets.push(lst);
            }
            Err(e) => err = Some(e),
        }
    }

    if !succ {
        if let Some(e) = err.take() {
            Err(e)
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Cannot bind to address.",
            ))
        }
    } else {
        Ok(sockets)
    }
}

pub fn create_tcp_listener(
    addr: net::SocketAddr,
    backlog: i32,
) -> io::Result<net::TcpListener> {
    let builder = match addr {
        net::SocketAddr::V4(_) => Socket::new(Domain::IPV4, Type::STREAM, None)?,
        net::SocketAddr::V6(_) => Socket::new(Domain::IPV6, Type::STREAM, None)?,
    };

    // On Windows, this allows rebinding sockets which are actively in use,
    // which allows “socket hijacking”, so we explicitly don't set it here.
    // https://docs.microsoft.com/en-us/windows/win32/winsock/using-so-reuseaddr-and-so-exclusiveaddruse
    #[cfg(not(windows))]
    builder.set_reuse_address(true)?;

    builder.bind(&SockAddr::from(addr))?;
    builder.listen(backlog)?;
    Ok(net::TcpListener::from(builder))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bind_addr() {
        let addrs: Vec<net::SocketAddr> = Vec::new();
        assert!(bind_addr(&addrs[..], 10).is_err());
    }

    #[test]
    fn test_debug() {
        let builder = ServerBuilder::default();
        assert!(format!("{builder:?}").contains("ServerBuilder"));
    }
}
