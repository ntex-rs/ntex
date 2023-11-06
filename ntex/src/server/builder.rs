use std::{fmt, future::Future, io, marker, mem, net, pin::Pin, task::Context, task::Poll};

use async_channel::unbounded;
use async_oneshot as oneshot;
use log::{error, info};
use socket2::{Domain, SockAddr, Socket, Type};

use crate::rt::{spawn, Signal, System};
use crate::time::{sleep, Millis};
use crate::{io::Io, service::ServiceFactory, util::join_all, util::Stream};

use super::accept::{AcceptLoop, AcceptNotify, Command};
use super::config::{
    Config, ConfigWrapper, ConfiguredService, ServiceConfig, ServiceRuntime,
};
use super::service::{Factory, InternalServiceFactory};
use super::worker::{self, Worker, WorkerAvailability, WorkerClient};
use super::{socket::Listener, Server, ServerCommand, ServerStatus, Token};

const STOP_DELAY: Millis = Millis(300);

type ServerStream = Pin<Box<dyn Stream<Item = ServerCommand>>>;

/// Server builder
pub struct ServerBuilder {
    threads: usize,
    token: Token,
    backlog: i32,
    workers: Vec<(usize, WorkerClient)>,
    services: Vec<Box<dyn InternalServiceFactory>>,
    sockets: Vec<(Token, String, Listener)>,
    accept: AcceptLoop,
    exit: bool,
    shutdown_timeout: Millis,
    no_signals: bool,
    cmd: ServerStream,
    server: Server,
    notify: Vec<oneshot::Sender<()>>,
}

impl Default for ServerBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl ServerBuilder {
    /// Create new Server builder instance
    pub fn new() -> ServerBuilder {
        let (tx, rx) = unbounded();
        let server = Server::new(tx);

        ServerBuilder {
            threads: std::thread::available_parallelism()
                .map_or(2, std::num::NonZeroUsize::get),
            token: Token(0),
            workers: Vec::new(),
            services: Vec::new(),
            sockets: Vec::new(),
            accept: AcceptLoop::new(server.clone()),
            backlog: 2048,
            exit: false,
            shutdown_timeout: Millis::from_secs(30),
            no_signals: false,
            cmd: Box::pin(rx),
            notify: Vec::new(),
            server,
        }
    }

    /// Set number of workers to start.
    ///
    /// By default server uses number of available logical cpu as workers
    /// count.
    pub fn workers(mut self, num: usize) -> Self {
        self.threads = num;
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
        worker::max_concurrent_connections(num);
        self
    }

    /// Stop ntex runtime when server get dropped.
    ///
    /// By default "stop runtime" is disabled.
    pub fn stop_runtime(mut self) -> Self {
        self.exit = true;
        self
    }

    /// Disable signal handling.
    ///
    /// By default signal handling is enabled.
    pub fn disable_signals(mut self) -> Self {
        self.no_signals = true;
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
        self.shutdown_timeout = timeout.into();
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

    /// Execute external configuration as part of the server building
    /// process.
    ///
    /// This function is useful for moving parts of configuration to a
    /// different module or even library.
    pub fn configure<F>(mut self, f: F) -> io::Result<ServerBuilder>
    where
        F: Fn(&mut ServiceConfig) -> io::Result<()>,
    {
        let mut cfg = ServiceConfig::new(self.threads, self.backlog);

        f(&mut cfg)?;

        let mut cfg = cfg.0.borrow_mut();
        let mut srv = ConfiguredService::new(cfg.apply.take().unwrap());
        for (name, lst) in mem::take(&mut cfg.services) {
            let token = self.token.next();
            srv.stream(token, name.clone(), lst.local_addr()?);
            self.sockets.push((token, name, Listener::from_tcp(lst)));
        }
        self.services.push(Box::new(srv));
        self.threads = cfg.threads;

        Ok(self)
    }

    /// Execute external async configuration as part of the server building
    /// process.
    ///
    /// This function is useful for moving parts of configuration to a
    /// different module or even library.
    pub async fn configure_async<F, R>(mut self, f: F) -> io::Result<ServerBuilder>
    where
        F: Fn(ServiceConfig) -> R,
        R: Future<Output = io::Result<()>>,
    {
        let cfg = ServiceConfig::new(self.threads, self.backlog);
        let inner = cfg.0.clone();

        f(cfg).await?;

        let mut cfg = inner.borrow_mut();
        let mut srv = ConfiguredService::new(cfg.apply.take().unwrap());
        for (name, lst) in mem::take(&mut cfg.services) {
            let token = self.token.next();
            srv.stream(token, name.clone(), lst.local_addr()?);
            self.sockets.push((token, name, Listener::from_tcp(lst)));
        }
        self.services.push(Box::new(srv));
        self.threads = cfg.threads;

        Ok(self)
    }

    /// Register async service configuration function.
    ///
    /// This function get called during worker runtime configuration stage.
    /// It get executed in the worker thread.
    pub fn on_worker_start<F, R, E>(mut self, f: F) -> Self
    where
        F: Fn(ServiceRuntime) -> R + Send + Clone + 'static,
        R: Future<Output = Result<(), E>> + 'static,
        E: fmt::Display + 'static,
    {
        self.services
            .push(Box::new(ConfiguredService::new(Box::new(ConfigWrapper {
                f,
                _t: marker::PhantomData,
            }))));
        self
    }

    /// Add new service to the server.
    pub fn bind<F, U, N: AsRef<str>, R>(
        mut self,
        name: N,
        addr: U,
        factory: F,
    ) -> io::Result<Self>
    where
        U: net::ToSocketAddrs,
        F: Fn(Config) -> R + Send + Clone + 'static,
        R: ServiceFactory<Io>,
    {
        let sockets = bind_addr(addr, self.backlog)?;

        for lst in sockets {
            let token = self.token.next();
            self.services.push(Factory::create(
                name.as_ref().to_string(),
                token,
                factory.clone(),
                lst.local_addr()?,
            ));
            self.sockets
                .push((token, name.as_ref().to_string(), Listener::from_tcp(lst)));
        }
        Ok(self)
    }

    #[cfg(unix)]
    /// Add new unix domain service to the server.
    pub fn bind_uds<F, U, N, R>(self, name: N, addr: U, factory: F) -> io::Result<Self>
    where
        N: AsRef<str>,
        U: AsRef<std::path::Path>,
        F: Fn(Config) -> R + Send + Clone + 'static,
        R: ServiceFactory<Io>,
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
        F: Fn(Config) -> R + Send + Clone + 'static,
        R: ServiceFactory<Io>,
    {
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};
        let token = self.token.next();
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        self.services.push(Factory::create(
            name.as_ref().to_string(),
            token,
            factory,
            addr,
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
        F: Fn(Config) -> R + Send + Clone + 'static,
        R: ServiceFactory<Io>,
    {
        let token = self.token.next();
        self.services.push(Factory::create(
            name.as_ref().to_string(),
            token,
            factory,
            lst.local_addr()?,
        ));
        self.sockets
            .push((token, name.as_ref().to_string(), Listener::from_tcp(lst)));
        Ok(self)
    }

    /// Starts processing incoming connections and return server controller.
    pub fn run(mut self) -> Server {
        if self.sockets.is_empty() {
            panic!("Server should have at least one bound socket");
        } else {
            info!("Starting {} workers", self.threads);

            // start workers
            let mut workers = Vec::new();
            for idx in 0..self.threads {
                let worker = self.start_worker(idx, self.accept.notify());
                workers.push(worker.clone());
                self.workers.push((idx, worker));
            }

            // start accept thread
            for sock in &self.sockets {
                info!("Starting \"{}\" service on {}", sock.1, sock.2);
            }
            self.accept.start(
                mem::take(&mut self.sockets)
                    .into_iter()
                    .map(|t| (t.0, t.2))
                    .collect(),
                workers,
            );

            // handle signals
            if !self.no_signals {
                spawn(signals(self.server.clone()));
            }

            // start http server actor
            let server = self.server.clone();
            spawn(self);
            server
        }
    }

    fn start_worker(&self, idx: usize, notify: AcceptNotify) -> WorkerClient {
        let avail = WorkerAvailability::new(notify);
        let services: Vec<Box<dyn InternalServiceFactory>> =
            self.services.iter().map(|v| v.clone_factory()).collect();

        Worker::start(idx, services, avail, self.shutdown_timeout)
    }

    fn handle_cmd(&mut self, item: ServerCommand) {
        match item {
            ServerCommand::Pause(mut tx) => {
                self.accept.send(Command::Pause);
                let _ = tx.send(());
            }
            ServerCommand::Resume(mut tx) => {
                self.accept.send(Command::Resume);
                let _ = tx.send(());
            }
            ServerCommand::Signal(sig) => {
                // Signals support
                // Handle `SIGINT`, `SIGTERM`, `SIGQUIT` signals and stop ntex system
                match sig {
                    Signal::Int => {
                        info!("SIGINT received, exiting");
                        self.exit = true;
                        self.handle_cmd(ServerCommand::Stop {
                            graceful: false,
                            completion: None,
                        })
                    }
                    Signal::Term => {
                        info!("SIGTERM received, stopping");
                        self.exit = true;
                        self.handle_cmd(ServerCommand::Stop {
                            graceful: true,
                            completion: None,
                        })
                    }
                    Signal::Quit => {
                        info!("SIGQUIT received, exiting");
                        self.exit = true;
                        self.handle_cmd(ServerCommand::Stop {
                            graceful: false,
                            completion: None,
                        })
                    }
                    _ => (),
                }
            }
            ServerCommand::Notify(tx) => {
                self.notify.push(tx);
            }
            ServerCommand::Stop {
                graceful,
                completion,
            } => {
                let exit = self.exit;

                // stop accept thread
                let (tx, rx) = std::sync::mpsc::channel();
                self.accept.send(Command::Stop(tx));
                let _ = rx.recv();

                let notify = std::mem::take(&mut self.notify);

                // stop workers
                if !self.workers.is_empty() && graceful {
                    let futs: Vec<_> = self
                        .workers
                        .iter()
                        .map(move |worker| worker.1.stop(graceful))
                        .collect();
                    spawn(async move {
                        let _ = join_all(futs).await;

                        if let Some(mut tx) = completion {
                            let _ = tx.send(());
                        }
                        for mut tx in notify {
                            let _ = tx.send(());
                        }
                        if exit {
                            sleep(STOP_DELAY).await;
                            System::current().stop();
                        }
                    });
                } else {
                    self.workers.iter().for_each(move |worker| {
                        worker.1.stop(false);
                    });

                    // we need to stop system if server was spawned
                    if self.exit {
                        spawn(async {
                            sleep(STOP_DELAY).await;
                            System::current().stop();
                        });
                    }
                    if let Some(mut tx) = completion {
                        let _ = tx.send(());
                    }
                    for mut tx in notify {
                        let _ = tx.send(());
                    }
                }
            }
            ServerCommand::WorkerFaulted(idx) => {
                let mut found = false;
                for i in 0..self.workers.len() {
                    if self.workers[i].0 == idx {
                        self.workers.swap_remove(i);
                        found = true;
                        break;
                    }
                }

                if found {
                    error!("Worker has died {:?}, restarting", idx);

                    let mut new_idx = self.workers.len();
                    'found: loop {
                        for i in 0..self.workers.len() {
                            if self.workers[i].0 == new_idx {
                                new_idx += 1;
                                continue 'found;
                            }
                        }
                        break;
                    }

                    let worker = self.start_worker(new_idx, self.accept.notify());
                    self.workers.push((new_idx, worker.clone()));
                    self.accept.send(Command::Worker(worker));
                }
            }
        }
    }
}

impl Future for ServerBuilder {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        loop {
            match Pin::new(&mut self.cmd).poll_next(cx) {
                Poll::Ready(Some(it)) => self.as_mut().get_mut().handle_cmd(it),
                Poll::Ready(None) => return Poll::Pending,
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

async fn signals(srv: Server) {
    loop {
        if let Some(rx) = crate::rt::signal() {
            if let Ok(sig) = rx.await {
                srv.signal(sig);
            } else {
                return;
            }
        } else {
            log::info!("Signals are not supported by current runtime");
            return;
        }
    }
}

pub(super) fn bind_addr<S: net::ToSocketAddrs>(
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
                io::ErrorKind::Other,
                "Cannot bind to address.",
            ))
        }
    } else {
        Ok(sockets)
    }
}

pub(crate) fn create_tcp_listener(
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
}
