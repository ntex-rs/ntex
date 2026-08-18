use std::{fmt, io, marker::PhantomData, net};

use ntex_io::Io;
use ntex_rt::System;
use ntex_service::{IntoService, Service, State, cfg::SharedCfg};
use ntex_util::time::Millis;
use socket2::{Domain, SockAddr, Socket, Type};

use crate::{Server, WorkerPool};

use super::accept::AcceptLoop;
use super::config::ServiceConfig;
use super::factory::{self, FactoryServiceType};
use super::state::{StateFactory, state_factory};
use super::{Connection, ServerStatus, StreamServer, Token, socket::Listener};

/// Streaming service builder
///
/// This type can be used to construct an instance of `net streaming server` through a
/// builder-like pattern.
pub struct ServerBuilder<St = ()> {
    name: String,
    token: Token,
    backlog: i32,
    state: Box<dyn StateFactory<St> + Send>,
    services: Vec<FactoryServiceType<St>>,
    sockets: Vec<(Token, String, Listener)>,
    accept: AcceptLoop,
    pool: WorkerPool,
    st: PhantomData<St>,
}

impl Default for ServerBuilder {
    fn default() -> Self {
        Self::new(async || Ok(()))
    }
}

impl<St> ServerBuilder<St>
where
    St: State<Io> + Clone + 'static,
{
    #[must_use]
    /// Create new Server builder instance.
    ///
    /// Provided function get called during worker runtime configuration stage
    /// and must construct server state.
    pub fn new<F>(state: F) -> ServerBuilder<St>
    where
        F: AsyncFn() -> Result<St, &'static str> + Send + Clone + 'static,
    {
        let sys = System::current();
        let mut accept = AcceptLoop::default();
        accept.name(sys.name());
        if sys.testing() {
            accept.testing();
        }

        ServerBuilder {
            accept,
            name: sys.name().to_string(),
            token: Token(0),
            state: state_factory(state),
            services: Vec::new(),
            sockets: Vec::new(),
            backlog: 2048,
            pool: WorkerPool::default().name(sys.name()),
            st: PhantomData,
        }
    }

    #[must_use]
    /// Create new Server builder instance with default state factory.
    pub fn with_default() -> ServerBuilder<St>
    where
        St: Default,
    {
        Self::new(async || Ok(St::default()))
    }

    #[must_use]
    /// Set server name.
    ///
    /// Name is used for worker thread name
    pub fn name<T: AsRef<str>>(mut self, name: T) -> Self {
        self.name = name.as_ref().to_string();
        self.accept.name(self.name.as_str());
        self.pool = self.pool.name(self.name.as_str());
        self
    }

    #[must_use]
    /// Set number of workers to start.
    ///
    /// By default server uses number of available logical cpu as workers
    /// count.
    pub fn workers(mut self, num: usize) -> Self {
        self.pool = self.pool.workers(num);
        self
    }

    #[must_use]
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

    #[must_use]
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

    #[must_use]
    /// Stop ntex runtime when server get dropped.
    ///
    /// By default "stop runtime" is disabled.
    pub fn stop_runtime(mut self) -> Self {
        self.pool = self.pool.stop_runtime();
        self
    }

    #[must_use]
    /// Stops the server when one of the workers panics.
    ///
    /// By default, "stop on panic" is disabled.
    pub fn stop_on_panic(mut self) -> Self {
        self.pool = self.pool.stop_on_panic();
        self
    }

    #[must_use]
    /// Disable signal handling.
    ///
    /// By default, signal handling is enabled.
    pub fn disable_signals(mut self) -> Self {
        self.pool = self.pool.disable_signals();
        self
    }

    #[must_use]
    /// Enable cpu affinity.
    ///
    /// By default, affinity is disabled.
    pub fn enable_affinity(mut self) -> Self {
        self.pool = self.pool.enable_affinity();
        self
    }

    #[must_use]
    /// Graceful shutdown.
    ///
    /// Gracefully shuts down on SIGTERM, SIGSEGV, or SIGQUIT.
    /// By default, graceful shutdown is disabled.
    pub fn graceful_shutdown(mut self) -> Self {
        self.pool = self.pool.graceful_shutdown();
        self
    }

    #[must_use]
    /// Timeout for graceful worker shutdown.
    ///
    /// After receiving a stop signal, workers have this much time to finish
    /// serving requests. Workers that are still alive after the timeout are
    /// forcefully dropped.
    ///
    /// By default, the shutdown timeout is set to 30 seconds.
    pub fn shutdown_timeout<T: Into<Millis>>(mut self, timeout: T) -> Self {
        self.pool = self.pool.shutdown_timeout(timeout);
        self
    }

    #[must_use]
    /// Sets the server status handler.
    ///
    /// The server calls this handler on every internal status update.
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
    pub async fn configure<F>(mut self, f: F) -> io::Result<Self>
    where
        F: AsyncFn(ServiceConfig<St>) -> io::Result<()>,
    {
        let cfg = ServiceConfig::new(self.token, self.backlog);

        f(cfg.clone()).await?;

        let (token, sockets, factory) = cfg.into_factory();
        self.token = token;
        self.sockets.extend(sockets);
        self.services.push(factory);

        Ok(self)
    }

    #[allow(clippy::needless_pass_by_value)]
    /// Add new service to the server.
    pub fn bind<F, S, I>(
        mut self,
        name: impl AsRef<str>,
        addr: impl net::ToSocketAddrs,
        cfg: impl Into<SharedCfg>,
        factory: F,
    ) -> io::Result<Self>
    where
        F: AsyncFn(&St) -> I + Send + Clone + 'static,
        S: Service<St, Req = Io> + 'static,
        I: IntoService<S, St> + 'static,
    {
        let cfg = cfg.into();
        let sockets = bind_addr(addr, self.backlog)?;

        let mut tokens = Vec::new();
        for lst in sockets {
            let token = self.token.next();
            self.sockets
                .push((token, name.as_ref().to_string(), Listener::from_tcp(lst)));
            tokens.push((token, cfg.clone()));
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
    pub fn bind_uds<F, S, I>(
        self,
        name: impl AsRef<str>,
        addr: impl AsRef<std::path::Path>,
        cfg: impl Into<SharedCfg>,
        factory: F,
    ) -> io::Result<Self>
    where
        F: AsyncFn(&St) -> I + Send + Clone + 'static,
        S: Service<St, Req = Io> + 'static,
        I: IntoService<S, St> + 'static,
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
        self.listen_uds(name, lst, cfg.into(), factory)
    }

    #[cfg(unix)]
    /// Add new unix domain service to the server.
    /// Useful when running as a systemd service and
    /// a socket FD can be acquired using the systemd crate.
    pub fn listen_uds<F, S, I>(
        mut self,
        name: impl AsRef<str>,
        lst: std::os::unix::net::UnixListener,
        cfg: impl Into<SharedCfg>,
        factory: F,
    ) -> io::Result<Self>
    where
        F: AsyncFn(&St) -> I + Send + Clone + 'static,
        S: Service<St, Req = Io> + 'static,
        I: IntoService<S, St> + 'static,
    {
        let token = self.token.next();
        self.services.push(factory::create_factory_service(
            name.as_ref().to_string(),
            vec![(token, cfg.into())],
            factory,
        ));
        self.sockets
            .push((token, name.as_ref().to_string(), Listener::from_uds(lst)));
        Ok(self)
    }

    /// Add new service to the server.
    pub fn listen<F, S, I>(
        mut self,
        name: impl AsRef<str>,
        lst: net::TcpListener,
        cfg: impl Into<SharedCfg>,
        factory: F,
    ) -> io::Result<Self>
    where
        F: AsyncFn(&St) -> I + Send + Clone + 'static,
        S: Service<St, Req = Io> + 'static,
        I: IntoService<S, St> + 'static,
    {
        let token = self.token.next();
        self.services.push(factory::create_factory_service(
            name.as_ref().to_string(),
            vec![(token, cfg.into())],
            factory,
        ));
        self.sockets
            .push((token, name.as_ref().to_string(), Listener::from_tcp(lst)));
        Ok(self)
    }

    /// Starts processing incoming connections and return server controller.
    pub fn run(self) -> Server<Connection> {
        assert!(
            !self.sockets.is_empty(),
            "Server should have at least one bound socket"
        );
        let srv = StreamServer::new(self.accept.notify(), self.state, self.services);
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

impl<St> fmt::Debug for ServerBuilder<St> {
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

    if succ {
        Ok(sockets)
    } else if let Some(e) = err.take() {
        Err(e)
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Cannot bind to address.",
        ))
    }
}

pub fn create_tcp_listener(addr: net::SocketAddr, backlog: i32) -> io::Result<net::TcpListener> {
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

    #[ntex::test]
    async fn test_debug() {
        let builder = ServerBuilder::default();
        assert!(format!("{builder:?}").contains("ServerBuilder"));
    }
}
