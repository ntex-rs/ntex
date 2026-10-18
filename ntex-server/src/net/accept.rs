#![allow(clippy::missing_panics_doc)]
use std::time::{Duration, Instant};
use std::{cell::Cell, fmt, io, sync::mpsc, thread};
use std::{collections::VecDeque, num::NonZeroUsize};

use ntex_polling::{Event, Events, Notifier, Poller};
use ntex_rt::System;
use ntex_util::{future::Either, time::Millis, time::sleep};

use super::socket::{Connection, Listener, SocketAddr};
use super::{Server, Token};
use crate::metrics::AcceptSlot;

const EXIT_TIMEOUT: Duration = Duration::from_millis(100);
const ERR_TIMEOUT: Duration = Duration::from_millis(500);
const ERR_SLEEP_TIMEOUT: Millis = Millis(525);

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

#[derive(Debug)]
struct ServerSocketInfo {
    addr: SocketAddr,
    token: Token,
    sock: Listener,
    registered: Cell<bool>,
    timeout: Cell<Option<Instant>>,
}

/// Handle for sending commands to the accept loop.
#[derive(Debug, Clone)]
pub struct AcceptNotify(Notifier, mpsc::Sender<AcceptorCommand>);

impl AcceptNotify {
    fn new(waker: Notifier, tx: mpsc::Sender<AcceptorCommand>) -> Self {
        AcceptNotify(waker, tx)
    }

    /// Sends a command and wakes the accept loop.
    pub fn send(&self, cmd: AcceptorCommand) {
        let _ = self.1.send(cmd);
        let _ = self.0.notify();
    }
}

/// Accept loop for network listeners.
///
/// The loop runs on its own thread and passes accepted connections to the
/// server.
pub struct AcceptLoop {
    name: String,
    testing: bool,
    notify: AcceptNotify,
    inner: Option<(mpsc::Receiver<AcceptorCommand>, Poller)>,
}

impl Default for AcceptLoop {
    fn default() -> Self {
        Self::new()
    }
}

impl AcceptLoop {
    /// Creates an accept loop.
    ///
    /// # Panics
    ///
    /// Panics if the poller cannot be created.
    pub fn new() -> AcceptLoop {
        // Create a poller instance
        let poll = Poller::new()
            .map_err(|e| panic!("Cannot create Poller {e}"))
            .unwrap();

        let (tx, rx) = mpsc::channel();
        let notify = AcceptNotify::new(poll.notifier(), tx);

        AcceptLoop {
            notify,
            name: "ntex:accept".to_string(),
            inner: Some((rx, poll)),
            testing: false,
        }
    }

    /// Sets the server name.
    ///
    /// The accept thread is named `{name}:accept`.
    pub fn name<T: AsRef<str>>(&mut self, name: T) {
        self.name = format!("{}:accept", name.as_ref());
    }

    /// Returns a handle for sending commands to the loop.
    pub fn notify(&self) -> AcceptNotify {
        self.notify.clone()
    }

    /// Enables test mode, which skips the exit delay on stop.
    pub fn testing(&mut self) {
        self.testing = true;
    }

    /// Starts the accept loop on a new thread.
    ///
    /// Blocks until the listeners are registered.
    pub fn start(mut self, socks: Vec<(Token, Listener)>, srv: Server) {
        let (tx, rx_start) = oneshot::channel();
        let (rx, poll) = self
            .inner
            .take()
            .expect("AcceptLoop cannot be used multiple times");

        Accept::start(
            tx,
            rx,
            poll,
            socks,
            srv,
            self.name.clone(),
            self.notify.clone(),
            self.testing,
        );

        let _ = rx_start.recv();
    }
}

impl fmt::Debug for AcceptLoop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AcceptLoop")
            .field("name", &self.name)
            .field("notify", &self.notify)
            .field("inner", &self.inner)
            .finish()
    }
}

struct Accept {
    name: String,
    poller: Poller,
    rx: mpsc::Receiver<AcceptorCommand>,
    tx: Option<oneshot::Sender<()>>,
    sockets: Vec<ServerSocketInfo>,
    srv: Server,
    notify: AcceptNotify,
    testing: bool,
    backpressure: bool,
    backlog: VecDeque<Connection>,
}

impl Accept {
    #[allow(clippy::too_many_arguments)]
    fn start(
        tx: oneshot::Sender<()>,
        rx: mpsc::Receiver<AcceptorCommand>,
        poller: Poller,
        socks: Vec<(Token, Listener)>,
        srv: Server,
        name: String,
        notify: AcceptNotify,
        testing: bool,
    ) {
        log::info!("Starting {name:?} accept loop");

        // start accept thread
        let sys = System::current();
        let _ = thread::Builder::new().name(name.clone()).spawn(move || {
            System::set_current(sys);
            Accept::new(name, tx, rx, poller, socks, srv, notify, testing).poll();
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn new(
        name: String,
        tx: oneshot::Sender<()>,
        rx: mpsc::Receiver<AcceptorCommand>,
        poller: Poller,
        socks: Vec<(Token, Listener)>,
        srv: Server,
        notify: AcceptNotify,
        testing: bool,
    ) -> Accept {
        let mut sockets = Vec::new();
        // the builder registers named listeners, others only have an address
        srv.metrics_ref().listeners(
            socks
                .iter()
                .map(|(_, lst)| (String::new(), lst.local_addr().to_string())),
        );
        for (hnd_token, lst) in socks {
            sockets.push(ServerSocketInfo {
                addr: lst.local_addr(),
                sock: lst,
                token: hnd_token,
                registered: Cell::new(false),
                timeout: Cell::new(None),
            });
        }

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
        }
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
                if self.sockets[idx].registered.get() {
                    let readd = self.accept(idx);
                    if readd {
                        self.add_source(idx);
                    }
                }
            }

            if let Either::Right(rx) = self.process_cmd() {
                // cleanup
                for info in self.sockets.drain(..) {
                    info.sock.remove_source();
                }
                log::info!("Accept loop {:?} has been stopped", self.name);

                if let Some(rx) = rx {
                    if !self.testing {
                        thread::sleep(EXIT_TIMEOUT);
                    }
                    let _ = rx.send(());
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
                System::current().handle().spawn(async move {
                    sleep(ERR_SLEEP_TIMEOUT).await;
                    notify.send(AcceptorCommand::Timer);
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

    fn process_cmd(&mut self) -> Either<(), Option<oneshot::Sender<()>>> {
        loop {
            match self.rx.try_recv() {
                Ok(cmd) => match cmd {
                    AcceptorCommand::Stop(rx) => {
                        if !self.backpressure {
                            log::info!("Stopping accept loop {:?}", self.name);
                            self.backpressure(true);
                        }
                        break Either::Right(Some(rx));
                    }
                    AcceptorCommand::Terminate => {
                        log::info!("Stopping accept loop {:?}", self.name);
                        self.backpressure(true);
                        break Either::Right(None);
                    }
                    AcceptorCommand::Pause => {
                        if !self.backpressure {
                            log::info!("Pausing accept loop {:?}", self.name);
                            self.backpressure(true);
                        }
                    }
                    AcceptorCommand::Resume => {
                        if self.backpressure {
                            log::info!("Resuming accept loop {:?}", self.name);
                            self.backpressure(false);
                        }
                    }
                    AcceptorCommand::Timer => {
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
                if let Err(msg) = self.srv.process(msg) {
                    log::trace!("Server is unavailable");
                    self.backlog.push_front(msg);
                    self.metrics().backlog(self.backlog.len());
                    return;
                }
            }
            self.metrics().backlog(0);
            self.metrics().paused(false);

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
            self.metrics().paused(true);
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
                match info.sock.accept() {
                    Ok(Some(io)) => {
                        self.srv.metrics_ref().listeners([]).accepted(token);
                        let msg = Connection {
                            io,
                            token: info.token,
                        };
                        if let Err(msg) = self.srv.process(msg) {
                            log::trace!("Server is unavailable");
                            self.backlog.push_back(msg);
                            self.metrics().backlog(self.backlog.len());
                            self.backpressure(true);
                            return false;
                        }
                    }
                    Ok(None) => return true,
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => return true,
                    Err(ref e) if connection_error(e) => {
                        self.srv.metrics_ref().listeners([]).connection_error(token);
                    }
                    Err(e) => {
                        log::error!("Error accepting socket: {e}");
                        self.srv.metrics_ref().listeners([]).resource_error(token);

                        // sleep after error
                        info.timeout.set(Some(Instant::now() + ERR_TIMEOUT));

                        let notify = self.notify.clone();
                        System::current().handle().spawn(async move {
                            sleep(ERR_SLEEP_TIMEOUT).await;
                            notify.send(AcceptorCommand::Timer);
                        });
                        return false;
                    }
                }
            }
        }
    }
}

/// This function defines errors that are per-connection. Which basically
/// means that if we get this error from `accept()` system call it means
/// next connection might be ready to be accepted.
///
/// All other errors will incur a timeout before next `accept()` is performed.
/// The timeout is useful to handle resource exhaustion errors like ENFILE
/// and EMFILE. Otherwise, could enter into tight loop.
fn connection_error(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
    )
}
