use std::{fmt, io, net, sync::Arc};

use ntex_io::Io;
use ntex_rt::System;
use ntex_service::{IntoService, Service, cfg::SharedCfg};
use ntex_util::time::Millis;
use socket2::{Domain, SockAddr, Socket, Type};

use crate::{Affinity, LoadBalancing, NoConfig, Server, ServerAppConfig, WorkerPool};

use super::accept::{AcceptLoop, replicate};
use super::config::ServiceConfig;
use super::factory::{self, FactoryServiceType};
use super::reuseport::{Reuseport, Sockets};
use super::{Connection, StreamServer, Token, socket::Listener};

/// Builder for a network server.
///
/// Register listeners and their service factories, configure the worker pool,
/// and call [`run`](Self::run) to start the server.
pub struct ServerBuilder<Cfg = NoConfig> {
    name: String,
    token: Token,
    backlog: i32,
    state: Arc<Cfg>,
    services: Vec<FactoryServiceType<Cfg>>,
    sockets: Vec<(Token, String, Listener)>,
    accept: AcceptLoop,
    acceptors: usize,
    max_conns: usize,
    pool: WorkerPool,
}

impl Default for ServerBuilder {
    fn default() -> Self {
        Self::new(NoConfig)
    }
}

impl<Cfg> ServerBuilder<Cfg>
where
    Cfg: ServerAppConfig,
{
    #[must_use]
    /// Creates a server builder with the specified application configuration.
    pub fn new(cfg: Cfg) -> ServerBuilder<Cfg> {
        let sys = System::current();
        let mut accept = AcceptLoop::default();
        accept.name(sys.name());
        if sys.testing() {
            accept.testing();
        }

        ServerBuilder {
            accept,
            acceptors: 1,
            max_conns: super::DEFAULT_MAX_CONNS,
            name: sys.name().to_string(),
            token: Token(0),
            state: Arc::new(cfg),
            services: Vec::new(),
            sockets: Vec::new(),
            backlog: 2048,
            pool: WorkerPool::default().name(sys.name()),
        }
    }

    #[must_use]
    /// Sets the server name.
    ///
    /// The name is also used for the accept and worker thread names. It
    /// defaults to the current system name.
    pub fn name<T: AsRef<str>>(mut self, name: T) -> Self {
        self.name = name.as_ref().to_string();
        self.accept.name(self.name.as_str());
        self.pool = self.pool.name(self.name.as_str());
        self
    }

    #[must_use]
    /// Sets the number of worker threads to start.
    ///
    /// By default, the server uses the number of available logical CPUs.
    ///
    /// # Panics
    ///
    /// Panics if `num` is zero.
    pub fn workers(mut self, num: usize) -> Self {
        self.pool = self.pool.workers(num);
        self
    }

    #[must_use]
    /// Sets the maximum number of pending connections.
    ///
    /// This refers to the number of clients that can be waiting to be served.
    /// Exceeding this number results in the client getting an error when
    /// attempting to connect. It should only affect servers under significant
    /// load.
    ///
    /// Generally set in the 64-2048 range. Default value is 2048.
    ///
    /// It applies to listeners created by later [`bind`](Self::bind) and
    /// [`configure`](Self::configure) calls. It does not affect listeners
    /// passed to [`listen`](Self::listen) or `listen_uds`.
    pub fn backlog(mut self, num: i32) -> Self {
        self.backlog = num;
        self.accept.backlog(num);
        self
    }

    #[must_use]
    /// Sets the number of accept threads.
    ///
    /// Each accept thread has its own `SO_REUSEPORT` copy of every TCP
    /// listener, and the kernel spreads incoming connections between them.
    /// This raises the connection rate the server can accept. Listeners
    /// passed to [`listen`](Self::listen) get `SO_REUSEPORT` enabled as well.
    /// Copies use the [`backlog`](Self::backlog) value. Unix domain listeners
    /// are served by a single accept thread.
    ///
    /// Multiple accept threads are supported on Linux and Android. On other
    /// platforms, a single thread is used.
    ///
    /// By default, the server uses a single accept thread. One thread is
    /// usually enough, add more only if the accept thread is saturated.
    pub fn acceptors(mut self, num: usize) -> Self {
        self.acceptors = num;
        self
    }

    #[must_use]
    /// Sets the maximum per-worker number of concurrent connections.
    ///
    /// A worker stops taking new connections while it is at this limit. When
    /// no worker can take a connection, the listeners stop accepting.
    ///
    /// The limit belongs to this server: each of its workers counts only
    /// connections for this server, and other servers in the process are not
    /// affected.
    ///
    /// The default is 25,600 connections per worker.
    pub fn max_connections(mut self, num: usize) -> Self {
        self.max_conns = num;
        self
    }

    #[must_use]
    /// Stops the current ntex runtime after the server has stopped.
    ///
    /// By default, "stop runtime" is disabled.
    pub fn stop_runtime(mut self) -> Self {
        self.pool = self.pool.stop_runtime();
        self
    }

    #[must_use]
    /// Stops the server when one of the workers fails.
    ///
    /// A worker fails when it panics or its service cannot be created. The
    /// stop is graceful only if [`graceful_shutdown`](Self::graceful_shutdown)
    /// is enabled. Without this option, a failed worker is restarted.
    ///
    /// By default, "stop on panic" is disabled.
    pub fn stop_on_panic(mut self) -> Self {
        self.pool = self.pool.stop_on_panic();
        self
    }

    #[must_use]
    /// Disables signal handling.
    ///
    /// By default, the server stops on SIGINT, SIGTERM, and SIGQUIT.
    pub fn disable_signals(mut self) -> Self {
        self.pool = self.pool.disable_signals();
        self
    }

    #[must_use]
    /// Sets the CPU affinity of worker threads.
    ///
    /// See [`WorkerPool::affinity`].
    pub fn affinity(mut self, affinity: Affinity) -> Self {
        self.pool = self.pool.affinity(affinity);
        self
    }

    #[must_use]
    /// Sets how new connections are distributed between workers.
    ///
    /// See [`WorkerPool::load_balancing`].
    pub fn load_balancing(mut self, balancing: LoadBalancing) -> Self {
        self.pool = self.pool.load_balancing(balancing);
        self
    }

    #[must_use]
    /// Enables graceful shutdown on SIGQUIT, fatal signals, and panics.
    ///
    /// When enabled, SIGQUIT, SIGSEGV, SIGABRT, application panics, and
    /// worker failures with "stop on panic" stop the server gracefully.
    /// SIGTERM always stops gracefully and SIGINT always stops immediately.
    /// SIGSEGV and SIGABRT require runtime fatal signal handling, application
    /// panics require runtime panic handling.
    ///
    /// By default, these events stop the server immediately.
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
    /// This bounds the worker as a whole, not an individual connection. Each
    /// connection is bound separately by `IoConfig::set_shutdown_timeout`, so
    /// this value should leave room for the connections a worker is still
    /// draining to shut down themselves.
    ///
    /// By default, the timeout is set to 30 seconds.
    pub fn graceful_shutdown_timeout<T: Into<Millis>>(mut self, timeout: T) -> Self {
        self.pool = self.pool.graceful_shutdown_timeout(timeout);
        self
    }

    /// Runs asynchronous configuration as part of the server building
    /// process.
    ///
    /// Listeners registered on the [`ServiceConfig`] are added to the server.
    /// Services for them are attached per worker in
    /// [`ServiceConfig::on_worker_start`]. This is useful for moving parts of
    /// the configuration to a different module or library.
    pub async fn configure<F>(mut self, f: F) -> io::Result<Self>
    where
        F: AsyncFnOnce(ServiceConfig<Cfg>) -> io::Result<()>,
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
    /// Binds TCP listeners and registers a service factory.
    ///
    /// A listener is created for every address resolved from `addr`. Binding
    /// succeeds if at least one of them binds; addresses that fail to bind
    /// are skipped.
    ///
    /// `cfg` is the I/O configuration for accepted connections. `factory` is
    /// called once per worker with that worker's application state and
    /// returns the connection service.
    pub fn bind<F, S, I>(
        mut self,
        name: impl AsRef<str>,
        addr: impl net::ToSocketAddrs,
        cfg: impl Into<SharedCfg>,
        factory: F,
    ) -> io::Result<Self>
    where
        F: AsyncFn(&Cfg::State) -> I + Send + Clone + 'static,
        S: Service<Cfg::State, Io> + 'static,
        I: IntoService<S, Cfg::State, Io> + 'static,
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
    /// Binds a Unix domain socket and registers a service factory.
    ///
    /// Any existing file at `addr` is removed before binding. The socket file
    /// is removed again when the server stops. See [`bind`](Self::bind) for
    /// `cfg` and `factory`.
    pub fn bind_uds<F, I, S>(
        self,
        name: impl AsRef<str>,
        addr: impl AsRef<std::path::Path>,
        cfg: impl Into<SharedCfg>,
        factory: F,
    ) -> io::Result<Self>
    where
        F: AsyncFn(&Cfg::State) -> I + Send + Clone + 'static,
        I: IntoService<S, Cfg::State, Io> + 'static,
        S: Service<Cfg::State, Io> + 'static,
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
    /// Registers a service factory for an existing Unix domain listener.
    ///
    /// This is useful for socket activation, including listeners acquired
    /// through systemd. The listener is switched to non-blocking mode. See
    /// [`bind`](Self::bind) for `cfg` and `factory`.
    pub fn listen_uds<F, I, S>(
        mut self,
        name: impl AsRef<str>,
        lst: std::os::unix::net::UnixListener,
        cfg: impl Into<SharedCfg>,
        factory: F,
    ) -> io::Result<Self>
    where
        F: AsyncFn(&Cfg::State) -> I + Send + Clone + 'static,
        I: IntoService<S, Cfg::State, Io> + 'static,
        S: Service<Cfg::State, Io> + 'static,
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

    /// Registers a service factory for an existing TCP listener.
    ///
    /// The listener is switched to non-blocking mode. See
    /// [`bind`](Self::bind) for `cfg` and `factory`.
    pub fn listen<F, S, I>(
        mut self,
        name: impl AsRef<str>,
        lst: net::TcpListener,
        cfg: impl Into<SharedCfg>,
        factory: F,
    ) -> io::Result<Self>
    where
        F: AsyncFn(&Cfg::State) -> I + Send + Clone + 'static,
        S: Service<Cfg::State, Io> + 'static,
        I: IntoService<S, Cfg::State, Io> + 'static,
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

    /// Starts processing incoming connections and returns a server controller.
    ///
    /// # Panics
    ///
    /// Panics if no listener has been registered.
    pub fn run(mut self) -> Server<Connection> {
        assert!(
            !self.sockets.is_empty(),
            "Server should have at least one bound socket"
        );
        self.accept.acceptors(self.acceptors);

        let names: Vec<_> = self
            .sockets
            .iter()
            .map(|sock| (sock.1.clone(), sock.2.local_addr().to_string()))
            .collect();
        let use_reuseport = self.pool.balancing == LoadBalancing::Reuseport && {
            let supported = cfg!(any(target_os = "linux", target_os = "android"))
                && ntex_net::is_tcp_listener_supported();
            if !supported {
                log::warn!("Reuseport load balancing is not supported, using an accept thread");
            }
            supported
        };

        let workers = self.pool.num;
        let mut slots: Vec<Sockets> = (0..workers).map(|_| Vec::new()).collect();
        let mut accept_socks = Vec::new();
        for (idx, (token, name, lst)) in self.sockets.into_iter().enumerate() {
            log::info!("Starting \"{name}\" service on {lst}");
            if use_reuseport && let Listener::Tcp(_) = lst {
                let copies = replicate(&lst, workers - 1, self.backlog);
                if copies.len() == workers - 1
                    && let Listener::Tcp(lst) = lst
                {
                    slots[0].push((idx, token, lst));
                    for (n, copy) in copies.into_iter().enumerate() {
                        match copy {
                            Listener::Tcp(copy) => slots[n + 1].push((idx, token, copy)),
                            // replicate() creates tcp listeners only
                            #[cfg(unix)]
                            Listener::Uds(_) => {}
                        }
                    }
                    continue;
                }
                log::warn!("Cannot replicate {lst}, using an accept thread");
            }
            accept_socks.push((idx, token, lst));
        }
        let reuseport = (use_reuseport && slots.iter().any(|s| !s.is_empty()))
            .then(|| Arc::new(Reuseport::new(slots)));

        let srv = StreamServer::new(
            self.accept.notify(),
            self.state,
            self.services,
            self.max_conns,
            reuseport.clone(),
        );
        let svc = self.pool.run(srv);

        svc.metrics_ref().listeners(names);
        if let Some(rp) = reuseport
            && let Some(metrics) = svc.metrics_ref().accept_slot()
        {
            rp.set_metrics(metrics);
        }
        if !accept_socks.is_empty() {
            self.accept.start_with(accept_socks, &svc);
        }

        svc
    }
}

impl<Cfg> fmt::Debug for ServerBuilder<Cfg> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerBuilder")
            .field("name", &self.name)
            .field("token", &self.token)
            .field("backlog", &self.backlog)
            .field("sockets", &self.sockets)
            .field("accept", &self.accept)
            .field("acceptors", &self.acceptors)
            .field("max_conns", &self.max_conns)
            .field("worker-pool", &self.pool)
            .finish()
    }
}

/// Binds TCP listeners for every address resolved from `addr`.
///
/// Succeeds if at least one address binds and returns only the listeners that
/// bound. Otherwise, returns the last bind error.
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

/// Creates and binds a TCP listener with the specified listen backlog.
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

    /// STATEMENT: A wakeup of a paused accept loop does not re-register its
    /// listeners, so connections stay in the listen backlog until resume.
    ///
    /// HOW:
    /// - Start a server, wait until it is ready, and pause it.
    /// - Wake the paused accept loop with `AcceptorCommand::Timer`.
    /// - Connect a client and check that the listener metrics count no
    ///   accepted connection after 200ms.
    /// - Resume the server and check that the connection is accepted.
    #[ntex::test]
    async fn test_paused_wakeup_does_not_accept() {
        use crate::ServerStatus;
        use crate::net::AcceptorCommand;
        use ntex_service::{cfg::SharedCfg, fn_service};

        let lst = net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = lst.local_addr().unwrap();
        let builder = ServerBuilder::default()
            .workers(1)
            .acceptors(1)
            .disable_signals()
            .listen("test", lst, SharedCfg::default(), async |&()| {
                fn_service(async |_: ntex_io::Io| Ok::<_, ()>(()))
            })
            .unwrap();
        let notify = builder.accept.notify();
        let srv = builder.run();
        ntex_util::time::timeout(Millis(3000), async {
            while srv.status() != ServerStatus::Ready {
                srv.status_changed().await;
            }
        })
        .await
        .unwrap();

        srv.pause().await;
        ntex_util::time::sleep(Millis(100)).await;
        notify.send(AcceptorCommand::Timer);
        ntex_util::time::sleep(Millis(100)).await;

        let _conn = net::TcpStream::connect(addr).unwrap();
        ntex_util::time::sleep(Millis(200)).await;
        assert_eq!(srv.metrics().listeners[0].accepted, 0);

        srv.resume().await;
        ntex_util::time::timeout(Millis(3000), async {
            while srv.metrics().listeners[0].accepted == 0 {
                ntex_util::time::sleep(Millis(10)).await;
            }
        })
        .await
        .unwrap();
        srv.stop(false).await;
    }
}
