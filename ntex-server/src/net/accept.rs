use std::time::{Duration, Instant};
use std::{cell::Cell, fmt, io, sync::mpsc, sync::Arc, thread};
use std::{collections::VecDeque, num::NonZeroUsize};

use ntex_rt::System;
use ntex_util::{future::Either, time::sleep, time::Millis};
use polling::{Event, Events, Poller};

use super::socket::{Connection, Listener, SocketAddr};
use super::{Server, ServerStatus, Token};

const EXIT_TIMEOUT: Duration = Duration::from_millis(100);
const ERR_TIMEOUT: Duration = Duration::from_millis(500);
const ERR_SLEEP_TIMEOUT: Millis = Millis(525);

#[derive(Debug)]
pub enum AcceptorCommand {
    Stop(oneshot::Sender<()>),
    Terminate,
    Pause,
    Resume,
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

#[derive(Debug, Clone)]
pub struct AcceptNotify(Arc<Poller>, mpsc::Sender<AcceptorCommand>);

impl AcceptNotify {
    fn new(waker: Arc<Poller>, tx: mpsc::Sender<AcceptorCommand>) -> Self {
        AcceptNotify(waker, tx)
    }

    pub fn send(&self, cmd: AcceptorCommand) {
        let _ = self.1.send(cmd);
        let _ = self.0.notify();
    }
}

/// Streamin io accept loop
pub struct AcceptLoop {
    notify: AcceptNotify,
    inner: Option<(mpsc::Receiver<AcceptorCommand>, Arc<Poller>)>,
    status_handler: Option<Box<dyn FnMut(ServerStatus) + Send>>,
}

impl Default for AcceptLoop {
    fn default() -> Self {
        Self::new()
    }
}

impl AcceptLoop {
    /// Create accept loop
    pub fn new() -> AcceptLoop {
        // Create a poller instance
        let poll = Arc::new(
            Poller::new()
                .map_err(|e| panic!("Cannot create Polller {}", e))
                .unwrap(),
        );

        let (tx, rx) = mpsc::channel();
        let notify = AcceptNotify::new(poll.clone(), tx);

        AcceptLoop {
            notify,
            inner: Some((rx, poll)),
            status_handler: None,
        }
    }

    /// Get notification api for the loop
    pub fn notify(&self) -> AcceptNotify {
        self.notify.clone()
    }

    pub fn set_status_handler<F>(&mut self, f: F)
    where
        F: FnMut(ServerStatus) + Send + 'static,
    {
        self.status_handler = Some(Box::new(f));
    }

    /// Start accept loop
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
            self.notify.clone(),
            self.status_handler.take(),
        );

        let _ = rx_start.recv();
    }
}

impl fmt::Debug for AcceptLoop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AcceptLoop")
            .field("notify", &self.notify)
            .field("inner", &self.inner)
            .field("status_handler", &self.status_handler.is_some())
            .finish()
    }
}

struct Accept {
    poller: Arc<Poller>,
    rx: mpsc::Receiver<AcceptorCommand>,
    tx: Option<oneshot::Sender<()>>,
    sockets: Vec<ServerSocketInfo>,
    srv: Server,
    notify: AcceptNotify,
    backpressure: bool,
    backlog: VecDeque<Connection>,
    status_handler: Option<Box<dyn FnMut(ServerStatus) + Send>>,
}

impl Accept {
    fn start(
        tx: oneshot::Sender<()>,
        rx: mpsc::Receiver<AcceptorCommand>,
        poller: Arc<Poller>,
        socks: Vec<(Token, Listener)>,
        srv: Server,
        notify: AcceptNotify,
        status_handler: Option<Box<dyn FnMut(ServerStatus) + Send>>,
    ) {
        let sys = System::current();

        // start accept thread
        let _ = thread::Builder::new()
            .name("ntex-server accept loop".to_owned())
            .spawn(move || {
                System::set_current(sys);
                Accept::new(tx, rx, poller, socks, srv, notify, status_handler).poll()
            });
    }

    fn new(
        tx: oneshot::Sender<()>,
        rx: mpsc::Receiver<AcceptorCommand>,
        poller: Arc<Poller>,
        socks: Vec<(Token, Listener)>,
        srv: Server,
        notify: AcceptNotify,
        status_handler: Option<Box<dyn FnMut(ServerStatus) + Send>>,
    ) -> Accept {
        let mut sockets = Vec::new();
        for (hnd_token, lst) in socks.into_iter() {
            sockets.push(ServerSocketInfo {
                addr: lst.local_addr(),
                sock: lst,
                token: hnd_token,
                registered: Cell::new(false),
                timeout: Cell::new(None),
            });
        }

        Accept {
            poller,
            rx,
            sockets,
            notify,
            srv,
            status_handler,
            tx: Some(tx),
            backpressure: true,
            backlog: VecDeque::new(),
        }
    }

    fn update_status(&mut self, st: ServerStatus) {
        if let Some(ref mut hnd) = self.status_handler {
            (*hnd)(st)
        }
    }

    fn poll(&mut self) {
        log::trace!("Starting server accept loop");

        // Create storage for events
        let mut events = Events::with_capacity(NonZeroUsize::new(512).unwrap());

        let mut timeout = Some(Duration::ZERO);
        loop {
            if let Err(e) = self.poller.wait(&mut events, timeout) {
                if e.kind() != io::ErrorKind::Interrupted {
                    panic!("Cannot wait for events in poller: {}", e)
                }
            } else if timeout.is_some() {
                timeout = None;
                let _ = self.tx.take().unwrap().send(());
            }

            for idx in 0..self.sockets.len() {
                if self.sockets[idx].registered.get() {
                    let readd = self.accept(idx);
                    if readd {
                        self.add_source(idx);
                    }
                }
            }

            match self.process_cmd() {
                Either::Left(_) => events.clear(),
                Either::Right(rx) => {
                    // cleanup
                    for info in self.sockets.drain(..) {
                        info.sock.remove_source()
                    }
                    log::info!("Accept loop has been stopped");

                    if let Some(rx) = rx {
                        thread::sleep(EXIT_TIMEOUT);
                        let _ = rx.send(());
                    }

                    break;
                }
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
                log::error!("Cannot register socket listener: {}", err);

                // sleep after error
                info.timeout.set(Some(Instant::now() + ERR_TIMEOUT));

                let notify = self.notify.clone();
                System::current().arbiter().spawn(Box::pin(async move {
                    sleep(ERR_SLEEP_TIMEOUT).await;
                    notify.send(AcceptorCommand::Timer);
                }));
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
            if let Some(inst) = info.timeout.get() {
                if now > inst && !self.backpressure {
                    log::info!("Resuming socket listener on {} after timeout", info.addr);
                    info.timeout.take();
                    self.add_source(key);
                }
            }
        }
    }

    fn process_cmd(&mut self) -> Either<(), Option<oneshot::Sender<()>>> {
        loop {
            match self.rx.try_recv() {
                Ok(cmd) => match cmd {
                    AcceptorCommand::Stop(rx) => {
                        if !self.backpressure {
                            log::info!("Stopping accept loop");
                            self.backpressure(true);
                        }
                        break Either::Right(Some(rx));
                    }
                    AcceptorCommand::Terminate => {
                        log::info!("Stopping accept loop");
                        self.backpressure(true);
                        break Either::Right(None);
                    }
                    AcceptorCommand::Pause => {
                        if !self.backpressure {
                            log::info!("Pausing accept loop");
                            self.backpressure(true);
                        }
                    }
                    AcceptorCommand::Resume => {
                        if self.backpressure {
                            log::info!("Resuming accept loop");
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
                            log::error!("Dropping accept loop");
                            self.backpressure(true);
                            Either::Right(None)
                        }
                    };
                }
            }
        }
    }

    fn backpressure(&mut self, on: bool) {
        self.update_status(if on {
            ServerStatus::NotReady
        } else {
            ServerStatus::Ready
        });

        if self.backpressure && !on {
            // handle backlog
            while let Some(msg) = self.backlog.pop_front() {
                if let Err(msg) = self.srv.process(msg) {
                    log::trace!("Server is unavailable");
                    self.backlog.push_front(msg);
                    return;
                }
            }

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
                        let msg = Connection {
                            io,
                            token: info.token,
                        };
                        if let Err(msg) = self.srv.process(msg) {
                            log::trace!("Server is unavailable");
                            self.backlog.push_back(msg);
                            self.backpressure(true);
                            return false;
                        }
                    }
                    Ok(None) => return true,
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => return true,
                    Err(ref e) if connection_error(e) => continue,
                    Err(e) => {
                        log::error!("Error accepting socket: {}", e);

                        // sleep after error
                        info.timeout.set(Some(Instant::now() + ERR_TIMEOUT));

                        let notify = self.notify.clone();
                        System::current().arbiter().spawn(Box::pin(async move {
                            sleep(ERR_SLEEP_TIMEOUT).await;
                            notify.send(AcceptorCommand::Timer);
                        }));
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
    e.kind() == io::ErrorKind::ConnectionRefused
        || e.kind() == io::ErrorKind::ConnectionAborted
        || e.kind() == io::ErrorKind::ConnectionReset
}
