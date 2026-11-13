#![allow(clippy::missing_panics_doc)]
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};
use std::{cell::Cell, fmt, io, thread};
use std::{collections::VecDeque, num::NonZeroUsize};

use ntex_polling::{Event, Events, Notifier, Poller};
use ntex_rt::System;
use ntex_util::{future::Either, time::Millis, time::sleep};
use parking_lot::Mutex;

use super::socket::{Connection, Listener, SocketAddr};
use super::{Server, Token};
use crate::metrics::AcceptSlot;

const EXIT_TIMEOUT: Duration = Duration::from_millis(100);
pub(super) const ERR_TIMEOUT: Duration = Duration::from_millis(500);
const ERR_SLEEP_TIMEOUT: Millis = Millis(525);
const DEFAULT_BACKLOG: i32 = 2048;

/// Command sent to the accept loop.
#[derive(Debug)]
pub enum AcceptorCommand {
    /// Stops accepting and closes the listeners, then signals the sender.
    Stop(oneshot::Sender<()>),
    /// Stops accepting and closes the listeners immediately.
    Terminate,
    /// Stops accepting connections.
    Pause,
    /// Resumes accepting connections.
    Resume,
    /// Re-registers listeners that were paused after an accept error.
    Timer,
}

/// Command delivered to a single accept thread.
#[derive(Debug, Clone)]
enum Command {
    Stop(Arc<StopSignal>),
    Terminate,
    Pause,
    Resume,
    Timer,
}

/// Completes a stop request once every accept thread has released it.
#[derive(Debug)]
struct StopSignal(Option<oneshot::Sender<()>>);

impl Drop for StopSignal {
    fn drop(&mut self) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(());
        }
    }
}

#[derive(Debug)]
struct ServerSocketInfo {
    addr: SocketAddr,
    token: Token,
    sock: Listener,
    /// Listener index used for metrics, shared by replicated sockets.
    idx: usize,
    registered: Cell<bool>,
    timeout: Cell<Option<Instant>>,
}

/// Command channel of a single accept thread.
#[derive(Debug, Clone)]
struct AcceptHandle(Notifier, mpsc::Sender<Command>);

impl AcceptHandle {
    fn send(&self, cmd: Command) {
        let _ = self.1.send(cmd);
        let _ = self.0.notify();
    }
}

/// Handle for sending commands to the accept loop.
///
/// Commands are delivered to every accept thread.
#[derive(Debug, Clone)]
pub struct AcceptNotify(Arc<Mutex<Vec<AcceptHandle>>>);

impl AcceptNotify {
    /// Sends a command and wakes the accept loop.
    ///
    /// A `Stop` command is signaled after every accept thread has stopped.
    pub fn send(&self, cmd: AcceptorCommand) {
        let handles = self.0.lock().clone();
        let cmd = match cmd {
            AcceptorCommand::Stop(tx) => Command::Stop(Arc::new(StopSignal(Some(tx)))),
            AcceptorCommand::Terminate => Command::Terminate,
            AcceptorCommand::Pause => Command::Pause,
            AcceptorCommand::Resume => Command::Resume,
            AcceptorCommand::Timer => Command::Timer,
        };
        for h in &handles {
            h.send(cmd.clone());
        }
    }
}

/// Accept loop for network listeners.
///
/// The loop runs on its own threads and passes accepted connections to the
/// server.
pub struct AcceptLoop {
    name: String,
    testing: bool,
    backlog: i32,
    notify: AcceptNotify,
    inner: Vec<(mpsc::Receiver<Command>, Poller)>,
}

impl Default for AcceptLoop {
    fn default() -> Self {
        Self::new()
    }
}

impl AcceptLoop {
    /// Creates an accept loop with one accept thread.
    ///
    /// # Panics
    ///
    /// Panics if the poller cannot be created.
    pub fn new() -> AcceptLoop {
        let mut slf = AcceptLoop {
            notify: AcceptNotify(Arc::new(Mutex::new(Vec::new()))),
            name: "ntex:accept".to_string(),
            inner: Vec::new(),
            testing: false,
            backlog: DEFAULT_BACKLOG,
        };
        slf.add_acceptor();
        slf
    }

    fn add_acceptor(&mut self) {
        let poll = Poller::new()
            .map_err(|e| panic!("Cannot create Poller {e}"))
            .unwrap();
        let (tx, rx) = mpsc::channel();
        self.notify.0.lock().push(AcceptHandle(poll.notifier(), tx));
        self.inner.push((rx, poll));
    }

    /// Sets the server name.
    ///
    /// The first accept thread is named `{name}:accept`, additional threads
    /// are named `{name}:accept{n}`.
    pub fn name<T: AsRef<str>>(&mut self, name: T) {
        self.name = format!("{}:accept", name.as_ref());
    }

    /// Sets the number of accept threads.
    ///
    /// Every TCP listener is replicated with `SO_REUSEPORT`, so each accept
    /// thread has its own socket for the same address and the kernel spreads
    /// incoming connections between them. Unix domain listeners are served
    /// by the first thread only.
    ///
    /// Multiple accept threads are supported on Linux and Android. On other
    /// platforms, a single thread is used.
    ///
    /// # Panics
    ///
    /// Panics if a poller cannot be created.
    pub fn acceptors(&mut self, num: usize) {
        let num = if cfg!(any(target_os = "linux", target_os = "android")) {
            num.max(1)
        } else {
            if num > 1 {
                log::warn!("Multiple accept threads are not supported on this platform");
            }
            1
        };
        while self.inner.len() < num {
            self.add_acceptor();
        }
        self.inner.truncate(num);
        self.notify.0.lock().truncate(num);
    }

    /// Sets the listen backlog for replicated listeners.
    pub(crate) fn backlog(&mut self, num: i32) {
        self.backlog = num;
    }

    /// Returns a handle for sending commands to the loop.
    pub fn notify(&self) -> AcceptNotify {
        self.notify.clone()
    }

    /// Enables test mode, which skips the exit delay on stop.
    pub fn testing(&mut self) {
        self.testing = true;
    }

    /// Starts the accept loop on new threads.
    ///
    /// Blocks until the listeners are registered.
    #[allow(clippy::needless_pass_by_value)]
    pub fn start(self, socks: Vec<(Token, Listener)>, srv: Server) {
        // the builder registers named listeners, others only have an address
        srv.metrics_ref().listeners(
            socks
                .iter()
                .map(|(_, lst)| (String::new(), lst.local_addr().to_string())),
        );
        let socks = socks
            .into_iter()
            .enumerate()
            .map(|(idx, (token, lst))| (idx, token, lst))
            .collect();
        self.start_with(socks, &srv);
    }

    /// Starts the accept loop for listeners with their metrics indexes.
    ///
    /// Blocks until the listeners are registered.
    pub(crate) fn start_with(self, socks: Vec<(usize, Token, Listener)>, srv: &Server) {
        let num = self.inner.len();
        let mut groups: Vec<Vec<(usize, Token, Listener)>> = (0..num).map(|_| Vec::new()).collect();
        for (idx, token, lst) in socks {
            if num > 1 {
                for (n, lst) in replicate(&lst, num - 1, self.backlog)
                    .into_iter()
                    .enumerate()
                {
                    groups[n + 1].push((idx, token, lst));
                }
            }
            groups[0].push((idx, token, lst));
        }

        let handles = self.notify.0.lock().clone();
        let mut started = Vec::new();
        for (n, ((rx, poll), socks)) in self.inner.into_iter().zip(groups).enumerate() {
            // the first thread always runs, others only serve replicated sockets
            if n > 0 && socks.is_empty() {
                continue;
            }
            let name = if n == 0 {
                self.name.clone()
            } else {
                format!("{}{n}", self.name)
            };
            let (tx, rx_start) = oneshot::channel();
            Accept::start(
                tx,
                rx,
                poll,
                handles[n].clone(),
                socks,
                srv.clone(),
                name,
                self.testing,
            );
            started.push(rx_start);
        }

        for rx in started {
            let _ = rx.recv();
        }
    }
}

/// Creates `num` more listeners bound to the address of `lst`.
///
/// `SO_REUSEPORT` is enabled on `lst` and on the new listeners, so the
/// kernel balances incoming connections between all of them.
pub(super) fn replicate(lst: &Listener, num: usize, backlog: i32) -> Vec<Listener> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    if let Listener::Tcp(lst) = lst {
        use socket2::{Domain, SockAddr, SockRef, Socket, Type};

        let create = || -> io::Result<std::net::TcpListener> {
            let addr = lst.local_addr()?;
            let sock = Socket::new(Domain::for_address(addr), Type::STREAM, None)?;
            sock.set_reuse_address(true)?;
            sock.set_reuse_port(true)?;
            if addr.is_ipv6() {
                sock.set_only_v6(SockRef::from(lst).only_v6()?)?;
            }
            sock.bind(&SockAddr::from(addr))?;
            sock.listen(backlog)?;
            Ok(sock.into())
        };

        if let Err(e) = SockRef::from(lst).set_reuse_port(true) {
            log::warn!("Cannot enable SO_REUSEPORT for {lst:?}: {e}");
            return Vec::new();
        }
        let mut items = Vec::with_capacity(num);
        for _ in 0..num {
            match create() {
                Ok(item) => items.push(Listener::from_tcp(item)),
                Err(e) => {
                    log::warn!("Cannot replicate listener {lst:?}: {e}");
                    break;
                }
            }
        }
        return items;
    }
    let _ = (lst, num, backlog);
    Vec::new()
}

impl fmt::Debug for AcceptLoop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AcceptLoop")
            .field("name", &self.name)
            .field("acceptors", &self.inner.len())
            .field("notify", &self.notify)
            .finish()
    }
}

struct Accept {
    name: String,
    poller: Poller,
    rx: mpsc::Receiver<Command>,
    tx: Option<oneshot::Sender<()>>,
    sockets: Vec<ServerSocketInfo>,
    srv: Server,
    notify: AcceptHandle,
    testing: bool,
    backpressure: bool,
    backlog: VecDeque<Connection>,
    /// Backlog length last added to the shared metrics.
    backlog_reported: usize,
    /// Whether this thread is counted as paused in the shared metrics.
    paused_reported: bool,
}

impl Accept {
    #[allow(clippy::too_many_arguments)]
    fn start(
        tx: oneshot::Sender<()>,
        rx: mpsc::Receiver<Command>,
        poller: Poller,
        notify: AcceptHandle,
        socks: Vec<(usize, Token, Listener)>,
        srv: Server,
        name: String,
        testing: bool,
    ) {
        log::info!("Starting {name:?} accept loop");

        // start accept thread
        let sys = System::current();
        let _ = thread::Builder::new().name(name.clone()).spawn(move || {
            System::set_current(sys);
            Accept::new(name, tx, rx, poller, notify, socks, srv, testing).poll();
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn new(
        name: String,
        tx: oneshot::Sender<()>,
        rx: mpsc::Receiver<Command>,
        poller: Poller,
        notify: AcceptHandle,
        socks: Vec<(usize, Token, Listener)>,
        srv: Server,
        testing: bool,
    ) -> Accept {
        let sockets = socks
            .into_iter()
            .map(|(idx, token, sock)| ServerSocketInfo {
                addr: sock.local_addr(),
                sock,
                idx,
                token,
                registered: Cell::new(false),
                timeout: Cell::new(None),
            })
            .collect();

        Accept {
            name,
            poller,
            rx,
            sockets,
            notify,
            srv,
            testing,
            tx: Some(tx),
            backpressure: true,
            backlog: VecDeque::new(),
            backlog_reported: 0,
            paused_reported: false,
        }
    }

    fn report_paused(&mut self, paused: bool) {
        if self.paused_reported != paused {
            self.paused_reported = paused;
            self.metrics().paused(paused);
        }
    }

    fn report_backlog(&mut self) {
        let len = self.backlog.len();
        self.metrics().backlog(self.backlog_reported, len);
        self.backlog_reported = len;
    }

    fn metrics(&self) -> &AcceptSlot {
        self.srv.metrics_ref().listeners([])
    }

    fn poll(mut self) {
        // Create storage for events
        let mut events = Events::with_capacity(NonZeroUsize::new(512).unwrap());

        // notify start
        for idx in 0..self.sockets.len() {
            self.add_source(idx);
        }
        if let Some(tx) = self.tx.take() {
            thread::sleep(Duration::from_millis(25));
            let _ = tx.send(());
        }

        loop {
            for idx in 0..self.sockets.len() {
                // paused sockets and sockets in error timeout must not be
                // polled, a wakeup would re-register them
                let info = &self.sockets[idx];
                if !self.backpressure && info.registered.get() && info.timeout.get().is_none() {
                    let readd = self.accept(idx);
                    if readd {
                        self.add_source(idx);
                    }
                }
            }

            if let Either::Right(rx) = self.process_cmd() {
                // cleanup
                for info in self.sockets.drain(..) {
                    delete_source(&self.poller, &info);
                    info.sock.remove_source();
                }
                log::info!("Accept loop {:?} has been stopped", self.name);

                // the stop request completes when every accept thread releases it
                if let Some(signal) = rx {
                    if !self.testing {
                        thread::sleep(EXIT_TIMEOUT);
                    }
                    drop(signal);
                }

                break;
            }

            events.clear();
            if let Err(e) = self.poller.wait(&mut events, None) {
                assert!(
                    e.kind() == io::ErrorKind::Interrupted,
                    "Cannot wait for events in poller: {e}"
                );
            }
        }
    }

    fn add_source(&self, idx: usize) {
        let info = &self.sockets[idx];

        loop {
            // try to register poller source
            let result = if info.registered.get() {
                self.poller.modify(&info.sock, Event::readable(idx))
            } else {
                // SAFETY: The listener stays in `self.sockets` and is deleted from the poller before drop.
                unsafe { self.poller.add(&info.sock, Event::readable(idx)) }
            };
            if let Err(err) = result {
                if err.kind() == io::ErrorKind::WouldBlock {
                    continue;
                }
                log::error!("Cannot register socket listener: {err}");

                // sleep after error
                info.timeout.set(Some(Instant::now() + ERR_TIMEOUT));

                let notify = self.notify.clone();
                // timers are thread-local, create the sleep on the system thread
                System::current().handle().spawn_detached(async move {
                    ntex_rt::spawn_detached(async move {
                        sleep(ERR_SLEEP_TIMEOUT).await;
                        notify.send(Command::Timer);
                    });
                });
            } else {
                info.registered.set(true);
            }

            break;
        }
    }

    fn remove_source(&self, key: usize) {
        let info = &self.sockets[key];

        let result = if info.registered.get() {
            self.poller.modify(&info.sock, Event::none(key))
        } else {
            return;
        };

        // stop listening for incoming connections
        if let Err(err) = result {
            log::error!("Cannot stop socket listener for {} err: {}", info.addr, err);
        }
    }

    fn process_timer(&mut self) {
        let now = Instant::now();
        for key in 0..self.sockets.len() {
            let info = &mut self.sockets[key];
            if let Some(inst) = info.timeout.get()
                && now > inst
                && !self.backpressure
            {
                log::info!("Resuming socket listener on {} after timeout", info.addr);
                info.timeout.take();
                self.add_source(key);
            }
        }
    }

    fn process_cmd(&mut self) -> Either<(), Option<Arc<StopSignal>>> {
        loop {
            match self.rx.try_recv() {
                Ok(cmd) => match cmd {
                    Command::Stop(rx) => {
                        if !self.backpressure {
                            log::info!("Stopping accept loop {:?}", self.name);
                            self.backpressure(true);
                        }
                        break Either::Right(Some(rx));
                    }
                    Command::Terminate => {
                        log::info!("Stopping accept loop {:?}", self.name);
                        self.backpressure(true);
                        break Either::Right(None);
                    }
                    Command::Pause => {
                        if !self.backpressure {
                            log::info!("Pausing accept loop {:?}", self.name);
                            self.backpressure(true);
                        }
                    }
                    Command::Resume => {
                        if self.backpressure {
                            log::info!("Resuming accept loop {:?}", self.name);
                            self.backpressure(false);
                        }
                    }
                    Command::Timer => {
                        self.process_timer();
                    }
                },
                Err(err) => {
                    break match err {
                        mpsc::TryRecvError::Empty => Either::Left(()),
                        mpsc::TryRecvError::Disconnected => {
                            log::error!("Dropping accept loop {:?}", self.name);
                            self.backpressure(true);
                            Either::Right(None)
                        }
                    };
                }
            }
        }
    }

    fn backpressure(&mut self, on: bool) {
        if self.backpressure && !on {
            // handle backlog
            while let Some(msg) = self.backlog.pop_front() {
                if let Err(msg) = self.srv.process_with(msg, Connection::incoming_cpu) {
                    log::trace!("Server is unavailable");
                    self.backlog.push_front(msg);
                    self.report_backlog();
                    return;
                }
            }
            self.report_backlog();
            self.report_paused(false);

            // re-enable acceptors
            self.backpressure = false;
            for (key, info) in self.sockets.iter().enumerate() {
                if info.timeout.get().is_none() {
                    // socket with timeout will re-register itself after timeout
                    log::info!(
                        "Resuming socket listener on {} after back-pressure",
                        info.addr
                    );
                    self.add_source(key);
                }
            }
        } else if !self.backpressure && on {
            self.backpressure = true;
            self.report_paused(true);
            for key in 0..self.sockets.len() {
                // disable err timeout
                let info = &mut self.sockets[key];
                if info.timeout.take().is_none() {
                    log::info!("Stopping socket listener on {}", info.addr);
                    self.remove_source(key);
                }
            }
        }
    }

    fn accept(&mut self, token: usize) -> bool {
        loop {
            if let Some(info) = self.sockets.get_mut(token) {
                let idx = info.idx;
                match info.sock.accept() {
                    Ok(Some(io)) => {
                        self.srv.metrics_ref().listeners([]).accepted(idx);
                        let msg = Connection {
                            io,
                            token: info.token,
                        };
                        if let Err(msg) = self.srv.process_with(msg, Connection::incoming_cpu) {
                            log::trace!("Server is unavailable");
                            self.backlog.push_back(msg);
                            self.report_backlog();
                            self.backpressure(true);
                            return false;
                        }
                    }
                    Ok(None) => return true,
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => return true,
                    Err(ref e) if connection_error(e) => {
                        self.srv.metrics_ref().listeners([]).connection_error(idx);
                    }
                    Err(e) => {
                        log::error!("Error accepting socket: {e}");
                        self.srv.metrics_ref().listeners([]).resource_error(idx);

                        // sleep after error
                        info.timeout.set(Some(Instant::now() + ERR_TIMEOUT));

                        let notify = self.notify.clone();
                        // timers are thread-local, create the sleep on the system thread
                        System::current().handle().spawn_detached(async move {
                            ntex_rt::spawn_detached(async move {
                                sleep(ERR_SLEEP_TIMEOUT).await;
                                notify.send(Command::Timer);
                            });
                        });
                        return false;
                    }
                }
            }
        }
    }
}

impl Drop for Accept {
    fn drop(&mut self) {
        // the normal shutdown path drains the sockets, this covers panics
        for info in &self.sockets {
            delete_source(&self.poller, info);
        }
    }
}

/// Deletes a registered listener from the poller, as `Poller::add` requires
/// before the listener is dropped.
fn delete_source(poller: &Poller, info: &ServerSocketInfo) {
    if info.registered.replace(false)
        && let Err(err) = poller.delete(&info.sock)
    {
        log::error!(
            "Cannot delete socket listener for {} err: {}",
            info.addr,
            err
        );
    }
}

/// This function defines errors that are per-connection. Which basically
/// means that if we get this error from `accept()` system call it means
/// next connection might be ready to be accepted.
///
/// All other errors will incur a timeout before next `accept()` is performed.
/// The timeout is useful to handle resource exhaustion errors like ENFILE
/// and EMFILE. Otherwise, could enter into tight loop.
pub(super) fn connection_error(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unused_async_trait_impl)]
    use std::sync::atomic::{AtomicUsize, Ordering};

    use ntex_service::{Ctx, Service};

    use super::*;
    use crate::{ServerConfiguration, ServerStatus, WorkerPool};

    /// Configuration whose services count the connections they receive.
    #[derive(Clone)]
    struct Cfg(Arc<AtomicUsize>);

    /// STATEMENT: `AcceptLoop` diagnostics show the thread name and the
    /// number of accept threads.
    ///
    /// HOW:
    /// - Create an accept loop and set the server name.
    /// - Check the debug output has the `{name}:accept` name, one acceptor
    ///   and the notify handles.
    #[test]
    fn debug_accept_loop() {
        let mut accept = AcceptLoop::new();
        accept.name("srv");
        let debug = format!("{accept:?}");
        assert!(
            debug.starts_with("AcceptLoop { name: \"srv:accept\", acceptors: 1, notify: "),
            "{debug}"
        );
    }

    /// STATEMENT: Only errors of a single connection let the accept loop
    /// continue at once, other errors pause the listener.
    ///
    /// HOW:
    /// - Check refused, aborted and reset connections are per-connection
    ///   errors.
    /// - Check resource and other errors are not.
    #[test]
    fn per_connection_errors() {
        use io::ErrorKind::*;

        for kind in [ConnectionRefused, ConnectionAborted, ConnectionReset] {
            assert!(connection_error(&kind.into()), "{kind:?}");
        }
        for kind in [OutOfMemory, WouldBlock, PermissionDenied, Other] {
            assert!(!connection_error(&kind.into()), "{kind:?}");
        }
    }

    struct Srv(Arc<AtomicUsize>);

    impl Service<(), Connection> for Srv {
        type Res = ();
        type Error = ();

        async fn call(&self, _: Connection, _: Ctx<'_, Self, ()>) -> Result<(), ()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    impl ServerConfiguration for Cfg {
        type Item = Connection;
        type Service = Srv;

        async fn create(&self) -> io::Result<Srv> {
            Ok(Srv(self.0.clone()))
        }
    }

    async fn wait_for(what: &str, f: impl Fn() -> bool) {
        for _ in 0..500 {
            if f() {
                return;
            }
            sleep(Millis(10)).await;
        }
        panic!("{what} is not met");
    }

    /// STATEMENT: Connections accepted while the server is unavailable are
    /// kept in the backlog until the server is ready again and the accept
    /// loop is resumed.
    ///
    /// HOW:
    /// - Start an accept loop with `AcceptLoop::start` for a server that
    ///   does not pause the loop itself.
    /// - Connect and check the connection is delivered only after the loop
    ///   is resumed, and the listener metrics report its address.
    /// - Pause the server, connect, and check the connection is kept in the
    ///   backlog and the accept loop is paused.
    /// - Resume the loop while the server is still paused and check the
    ///   connection stays in the backlog.
    /// - Resume the server and the loop and check the queued and a new
    ///   connection are delivered, and the listener metrics count three
    ///   accepted connections.
    /// - Terminate the loop and check the listener is closed.
    #[ntex::test]
    async fn backlog_while_server_unavailable() {
        let received = Arc::new(AtomicUsize::new(0));
        let srv = WorkerPool::new()
            .name("test")
            .workers(1)
            .disable_signals()
            .run(Cfg(received.clone()));
        while srv.status() != ServerStatus::Ready {
            srv.status_changed().await;
        }

        let lst = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = lst.local_addr().unwrap();
        let mut accept = AcceptLoop::new();
        accept.name("test");
        accept.testing();
        let notify = accept.notify();
        accept.start(vec![(Token(0), Listener::from_tcp(lst))], srv.clone());

        // the loop starts paused until the server is ready
        let _c1 = std::net::TcpStream::connect(addr).unwrap();
        sleep(Millis(100)).await;
        assert_eq!(received.load(Ordering::SeqCst), 0);
        notify.send(AcceptorCommand::Resume);
        wait_for("first connection", || received.load(Ordering::SeqCst) == 1).await;
        let m = srv.metrics();
        assert_eq!(m.listeners[0].addr, addr.to_string());
        assert_eq!(m.listeners[0].accepted, 1);

        srv.pause().await;
        let _c2 = std::net::TcpStream::connect(addr).unwrap();
        wait_for("backlog", || srv.metrics().accept_backlog == 1).await;
        assert_eq!(srv.metrics().accept_pauses, 1);

        notify.send(AcceptorCommand::Resume);
        sleep(Millis(100)).await;
        assert_eq!(srv.metrics().accept_backlog, 1);
        assert_eq!(received.load(Ordering::SeqCst), 1);

        srv.resume().await;
        notify.send(AcceptorCommand::Resume);
        wait_for("backlog connection", || {
            received.load(Ordering::SeqCst) == 2
        })
        .await;
        assert_eq!(srv.metrics().accept_backlog, 0);
        let _c3 = std::net::TcpStream::connect(addr).unwrap();
        wait_for("third connection", || received.load(Ordering::SeqCst) == 3).await;
        assert_eq!(srv.metrics().listeners[0].accepted, 3);

        // probe connections may be accepted before the loop handles `Terminate`
        notify.send(AcceptorCommand::Terminate);
        wait_for("closed listener", || {
            std::net::TcpStream::connect(addr).is_err()
        })
        .await;
        srv.stop(false).await;
    }
}
