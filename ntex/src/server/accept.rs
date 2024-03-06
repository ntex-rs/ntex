use std::time::{Duration, Instant};
use std::{cell::Cell, fmt, io, num::NonZeroUsize, sync::mpsc, sync::Arc, thread};

use polling::{Event, Events, Poller};

use crate::{rt::System, time::sleep, time::Millis, util::Either};

use super::socket::{Listener, SocketAddr, Stream};
use super::worker::{WorkerClient, WorkerManagerCmd, WorkerManagerNotifier, WorkerMessage};
use super::{Server, ServerStatus, Token};

const EXIT_TIMEOUT: Duration = Duration::from_millis(100);
const ERR_TIMEOUT: Duration = Duration::from_millis(500);
const ERR_SLEEP_TIMEOUT: Millis = Millis(525);

#[derive(Debug)]
struct ServerSocketInfo {
    addr: SocketAddr,
    token: Token,
    sock: Listener,
    registered: Cell<bool>,
    timeout: Cell<Option<Instant>>,
}

#[derive(Debug, Clone)]
pub(super) struct AcceptNotify(Arc<Poller>, mpsc::Sender<WorkerManagerCmd<Stream>>);

impl AcceptNotify {
    pub(super) fn new(
        waker: Arc<Poller>,
        tx: mpsc::Sender<WorkerManagerCmd<Stream>>,
    ) -> Self {
        AcceptNotify(waker, tx)
    }
}

impl WorkerManagerNotifier<Stream> for AcceptNotify {
    fn send(&self, cmd: WorkerManagerCmd<Stream>) {
        let _ = self.1.send(cmd);
        let _ = self.0.notify();
    }

    fn clone_box(&self) -> Box<dyn WorkerManagerNotifier<Stream>> {
        Box::new(self.clone())
    }
}

pub(super) struct AcceptLoop {
    notify: AcceptNotify,
    inner: Option<(
        mpsc::Receiver<WorkerManagerCmd<Stream>>,
        Arc<Poller>,
        Server,
    )>,
    status_handler: Option<Box<dyn FnMut(ServerStatus) + Send>>,
}

impl AcceptLoop {
    pub(super) fn new(srv: Server) -> AcceptLoop {
        // Create a poller instance
        let poll = Arc::new(
            Poller::new()
                .map_err(|e| panic!("Cannot create Poller {}", e))
                .unwrap(),
        );

        let (tx, rx) = mpsc::channel();
        let notify = AcceptNotify::new(poll.clone(), tx);

        AcceptLoop {
            notify,
            inner: Some((rx, poll, srv)),
            status_handler: None,
        }
    }

    pub(super) fn send(&self, msg: WorkerManagerCmd<Stream>) {
        self.notify.send(msg)
    }

    pub(super) fn notify(&self) -> AcceptNotify {
        self.notify.clone()
    }

    pub(super) fn set_status_handler<F>(&mut self, f: F)
    where
        F: FnMut(ServerStatus) + Send + 'static,
    {
        self.status_handler = Some(Box::new(f));
    }

    pub(super) fn start(
        &mut self,
        socks: Vec<(Token, Listener)>,
        workers: Vec<WorkerClient<Stream>>,
    ) {
        let (rx, poll, srv) = self
            .inner
            .take()
            .expect("AcceptLoop cannot be used multiple times");
        let status_handler = self.status_handler.take();

        Accept::start(
            rx,
            poll,
            socks,
            srv,
            workers,
            self.notify.clone(),
            status_handler,
        );
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
    rx: mpsc::Receiver<WorkerManagerCmd<Stream>>,
    sockets: Vec<ServerSocketInfo>,
    workers: Vec<WorkerClient<Stream>>,
    srv: Server,
    notify: AcceptNotify,
    next: usize,
    backpressure: bool,
    status_handler: Option<Box<dyn FnMut(ServerStatus) + Send>>,
}

impl Accept {
    fn start(
        rx: mpsc::Receiver<WorkerManagerCmd<Stream>>,
        poller: Arc<Poller>,
        socks: Vec<(Token, Listener)>,
        srv: Server,
        workers: Vec<WorkerClient<Stream>>,
        notify: AcceptNotify,
        status_handler: Option<Box<dyn FnMut(ServerStatus) + Send>>,
    ) {
        let sys = System::current();

        // start accept thread
        let _ = thread::Builder::new()
            .name("ntex-server accept loop".to_owned())
            .spawn(move || {
                System::set_current(sys);
                Accept::new(rx, poller, socks, workers, srv, notify, status_handler).poll()
            });
    }

    fn new(
        rx: mpsc::Receiver<WorkerManagerCmd<Stream>>,
        poller: Arc<Poller>,
        socks: Vec<(Token, Listener)>,
        workers: Vec<WorkerClient<Stream>>,
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
            workers,
            notify,
            srv,
            status_handler,
            next: 0,
            backpressure: false,
        }
    }

    fn update_status(&mut self, st: ServerStatus) {
        if let Some(ref mut hnd) = self.status_handler {
            (*hnd)(st)
        }
    }

    fn poll(&mut self) {
        log::trace!("Starting server accept loop");

        // Add all sources
        for (idx, info) in self.sockets.iter().enumerate() {
            log::info!("Starting socket listener on {}", info.addr);
            self.add_source(idx);
        }

        // Create storage for events
        let mut events = Events::with_capacity(NonZeroUsize::new(512).unwrap());

        loop {
            if let Err(e) = self.poller.wait(&mut events, None) {
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                } else {
                    panic!("Cannot wait for events in poller: {}", e)
                }
            }

            for event in events.iter() {
                let readd = self.accept(event.key);
                if readd {
                    self.add_source(event.key);
                }
            }

            match self.process_cmd() {
                Either::Left(_) => events.clear(),
                Either::Right(rx) => {
                    // cleanup
                    for info in self.sockets.drain(..) {
                        info.sock.remove_source()
                    }

                    if let Some(rx) = rx {
                        thread::sleep(EXIT_TIMEOUT);
                        let _ = rx.send(());
                    }

                    log::trace!("Accept loop has been stopped");
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
                    notify.send(WorkerManagerCmd::Timer);
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

    fn process_cmd(&mut self) -> Either<(), Option<mpsc::Sender<()>>> {
        loop {
            match self.rx.try_recv() {
                Ok(cmd) => match cmd {
                    WorkerManagerCmd::Stop(rx) => {
                        log::trace!("Stopping accept loop");
                        for (key, info) in self.sockets.iter().enumerate() {
                            log::info!("Stopping socket listener on {}", info.addr);
                            self.remove_source(key);
                        }
                        self.update_status(ServerStatus::NotReady);
                        break Either::Right(Some(rx));
                    }
                    WorkerManagerCmd::Pause => {
                        log::trace!("Pausing accept loop");
                        for (key, info) in self.sockets.iter().enumerate() {
                            log::info!("Stopping socket listener on {}", info.addr);
                            self.remove_source(key);
                        }
                        self.update_status(ServerStatus::NotReady);
                    }
                    WorkerManagerCmd::Resume => {
                        log::trace!("Resuming accept loop");
                        for (key, info) in self.sockets.iter().enumerate() {
                            log::info!("Resuming socket listener on {}", info.addr);
                            self.add_source(key);
                        }
                        self.update_status(ServerStatus::Ready);
                    }
                    WorkerManagerCmd::Worker(worker) => {
                        log::trace!("Adding new worker to accept loop");
                        self.backpressure(false);
                        self.workers.push(worker);
                    }
                    WorkerManagerCmd::Timer => {
                        self.process_timer();
                    }
                    WorkerManagerCmd::WorkerAvailable => {
                        log::trace!("Worker is available");
                        self.backpressure(false);
                    }
                },
                Err(err) => {
                    break match err {
                        mpsc::TryRecvError::Empty => Either::Left(()),
                        mpsc::TryRecvError::Disconnected => {
                            for (key, info) in self.sockets.iter().enumerate() {
                                log::info!("Stopping socket listener on {}", info.addr);
                                self.remove_source(key);
                            }

                            Either::Right(None)
                        }
                    }
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

        if self.backpressure {
            if !on {
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
            }
        } else if on {
            self.backpressure = true;
            for key in 0..self.sockets.len() {
                // disable err timeout
                let info = &mut self.sockets[key];
                if info.timeout.take().is_none() {
                    log::trace!("Enabling back-pressure for {}", info.addr);
                    self.remove_source(key);
                }
            }
        }
    }

    fn accept_one(&mut self, mut msg: WorkerMessage<Stream>) {
        log::trace!(
            "Accepting connection: {:?} bp: {}",
            msg.content,
            self.backpressure
        );

        if self.backpressure {
            while !self.workers.is_empty() {
                match self.workers[self.next].send(msg) {
                    Ok(_) => (),
                    Err(tmp) => {
                        log::trace!("Worker failed while processing connection");
                        self.update_status(ServerStatus::WorkerFailed);
                        self.srv.worker_faulted(self.workers[self.next].idx);
                        msg = tmp;
                        self.workers.swap_remove(self.next);
                        if self.workers.is_empty() {
                            log::error!("No workers");
                            return;
                        } else if self.workers.len() <= self.next {
                            self.next = 0;
                        }
                        continue;
                    }
                }
                self.next = (self.next + 1) % self.workers.len();
                break;
            }
        } else {
            let mut idx = 0;
            while idx < self.workers.len() {
                idx += 1;
                if self.workers[self.next].available() {
                    match self.workers[self.next].send(msg) {
                        Ok(_) => {
                            log::trace!("Sent to worker {:?}", self.next);
                            self.next = (self.next + 1) % self.workers.len();
                            return;
                        }
                        Err(tmp) => {
                            log::trace!("Worker failed while processing connection");
                            self.update_status(ServerStatus::WorkerFailed);
                            self.srv.worker_faulted(self.workers[self.next].idx);
                            msg = tmp;
                            self.workers.swap_remove(self.next);
                            if self.workers.is_empty() {
                                log::error!("No workers");
                                self.backpressure(true);
                                return;
                            } else if self.workers.len() <= self.next {
                                self.next = 0;
                            }
                            continue;
                        }
                    }
                }
                self.next = (self.next + 1) % self.workers.len();
            }
            // enable backpressure
            log::trace!("No available workers, enable back-pressure");
            self.backpressure(true);
            self.accept_one(msg);
        }
    }

    fn accept(&mut self, token: usize) -> bool {
        loop {
            let msg = if let Some(info) = self.sockets.get_mut(token) {
                match info.sock.accept() {
                    Ok(Some(io)) => WorkerMessage {
                        content: io,
                        token: info.token,
                    },
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
                            notify.send(WorkerManagerCmd::Timer);
                        }));
                        return false;
                    }
                }
            } else {
                return false;
            };

            self.accept_one(msg);
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
