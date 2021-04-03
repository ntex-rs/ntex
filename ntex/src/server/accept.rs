use std::{io, sync::mpsc as sync_mpsc, sync::Arc, thread, time::Duration};

use log::{error, info};
use slab::Slab;

use crate::rt::time::{sleep_until, Instant};
use crate::rt::System;

use super::socket::{Listener, SocketAddr};
use super::worker::{Connection, WorkerClient};
use super::{Server, Token};

const DELTA: usize = 100;
const NOTIFY: mio::Token = mio::Token(0);
const ERR_TIMEOUT: Duration = Duration::from_millis(500);
const ERR_SLEEP_TIMEOUT: Duration = Duration::from_millis(525);

#[derive(Debug)]
pub(super) enum Command {
    Pause,
    Resume,
    Stop,
    Worker(WorkerClient),
    Timer,
    WorkerAvailable,
}

struct ServerSocketInfo {
    addr: SocketAddr,
    token: Token,
    sock: Listener,
    timeout: Option<Instant>,
}

#[derive(Debug, Clone)]
pub(super) struct AcceptNotify(Arc<mio::Waker>, sync_mpsc::Sender<Command>);

impl AcceptNotify {
    pub(super) fn new(waker: Arc<mio::Waker>, tx: sync_mpsc::Sender<Command>) -> Self {
        AcceptNotify(waker, tx)
    }

    pub(super) fn send(&self, cmd: Command) {
        let _ = self.1.send(cmd);
        let _ = self.0.wake();
    }
}

pub(super) struct AcceptLoop {
    notify: AcceptNotify,
    inner: Option<(sync_mpsc::Receiver<Command>, mio::Poll, Server)>,
}

impl AcceptLoop {
    pub(super) fn new(srv: Server) -> AcceptLoop {
        // Create a poll instance
        let poll = mio::Poll::new()
            .map_err(|e| panic!("Cannot create mio::Poll {}", e))
            .unwrap();

        let (tx, rx) = sync_mpsc::channel();
        let waker = Arc::new(
            mio::Waker::new(poll.registry(), NOTIFY)
                .map_err(|e| panic!("Cannot create mio::Waker {}", e))
                .unwrap(),
        );
        let notify = AcceptNotify::new(waker, tx);

        AcceptLoop {
            notify,
            inner: Some((rx, poll, srv)),
        }
    }

    pub(super) fn send(&self, msg: Command) {
        self.notify.send(msg)
    }

    pub(super) fn notify(&self) -> AcceptNotify {
        self.notify.clone()
    }

    pub(super) fn start(
        &mut self,
        socks: Vec<(Token, Listener)>,
        workers: Vec<WorkerClient>,
    ) {
        let (rx, poll, srv) = self
            .inner
            .take()
            .expect("AcceptLoop cannot be used multiple times");

        Accept::start(rx, poll, socks, srv, workers, self.notify.clone());
    }
}

struct Accept {
    poll: mio::Poll,
    rx: sync_mpsc::Receiver<Command>,
    sockets: Slab<ServerSocketInfo>,
    workers: Vec<WorkerClient>,
    srv: Server,
    notify: AcceptNotify,
    next: usize,
    backpressure: bool,
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

impl Accept {
    fn start(
        rx: sync_mpsc::Receiver<Command>,
        poll: mio::Poll,
        socks: Vec<(Token, Listener)>,
        srv: Server,
        workers: Vec<WorkerClient>,
        notify: AcceptNotify,
    ) {
        let sys = System::current();

        // start accept thread
        let _ = thread::Builder::new()
            .name("ntex-server accept loop".to_owned())
            .spawn(move || {
                System::set_current(sys);
                Accept::new(rx, poll, socks, workers, srv, notify).poll()
            });
    }

    fn new(
        rx: sync_mpsc::Receiver<Command>,
        poll: mio::Poll,
        socks: Vec<(Token, Listener)>,
        workers: Vec<WorkerClient>,
        srv: Server,
        notify: AcceptNotify,
    ) -> Accept {
        // Start accept
        let mut sockets = Slab::new();
        for (hnd_token, mut lst) in socks.into_iter() {
            let addr = lst.local_addr();
            let entry = sockets.vacant_entry();
            let token = entry.key();

            // Start listening for incoming connections
            if let Err(err) = poll.registry().register(
                &mut lst,
                mio::Token(token + DELTA),
                mio::Interest::READABLE,
            ) {
                panic!("Cannot register io: {}", err);
            }

            entry.insert(ServerSocketInfo {
                addr,
                sock: lst,
                token: hnd_token,
                timeout: None,
            });
        }

        Accept {
            poll,
            rx,
            sockets,
            workers,
            notify,
            srv,
            next: 0,
            backpressure: false,
        }
    }

    fn poll(&mut self) {
        trace!("Starting server accept loop");

        // Create storage for events
        let mut events = mio::Events::with_capacity(128);

        loop {
            if let Err(e) = self.poll.poll(&mut events, None) {
                match e.kind() {
                    std::io::ErrorKind::Interrupted => {
                        continue;
                    }
                    _ => {
                        panic!("Poll error: {}", e);
                    }
                }
            }

            for event in events.iter() {
                let token = event.token();
                match token {
                    NOTIFY => {
                        if !self.process_cmd() {
                            return;
                        }
                    }
                    _ => {
                        let token = usize::from(token);
                        if token < DELTA {
                            continue;
                        }
                        self.accept(token - DELTA);
                    }
                }
            }
        }
    }

    fn process_timer(&mut self) {
        let now = Instant::now();
        for (token, info) in self.sockets.iter_mut() {
            if let Some(inst) = info.timeout.take() {
                if now > inst {
                    if !self.backpressure {
                        if let Err(err) = self.poll.registry().register(
                            &mut info.sock,
                            mio::Token(token + DELTA),
                            mio::Interest::READABLE,
                        ) {
                            error!("Cannot register server socket {}", err);
                        } else {
                            info!("Resume accepting connections on {}", info.addr);
                        }
                    }
                } else {
                    info.timeout = Some(inst);
                    break;
                }
            }
        }
    }

    fn process_cmd(&mut self) -> bool {
        loop {
            match self.rx.try_recv() {
                Ok(cmd) => match cmd {
                    Command::Pause => {
                        for (_, info) in self.sockets.iter_mut() {
                            if let Err(err) =
                                self.poll.registry().deregister(&mut info.sock)
                            {
                                error!("Cannot deregister server socket {}", err);
                            } else {
                                info!("Paused accepting connections on {}", info.addr);
                            }
                        }
                    }
                    Command::Resume => {
                        for (token, info) in self.sockets.iter_mut() {
                            if let Err(err) = self.poll.registry().register(
                                &mut info.sock,
                                mio::Token(token + DELTA),
                                mio::Interest::READABLE,
                            ) {
                                error!("Cannot resume socket accept process: {}", err);
                            } else {
                                info!(
                                    "Accepting connections on {} has been resumed",
                                    info.addr
                                );
                            }
                        }
                    }
                    Command::Stop => {
                        for (_, info) in self.sockets.iter_mut() {
                            trace!("Stopping socket listener: {}", info.addr);
                            let _ = self.poll.registry().deregister(&mut info.sock);
                        }
                        return false;
                    }
                    Command::Worker(worker) => {
                        self.backpressure(false);
                        self.workers.push(worker);
                    }
                    Command::Timer => {
                        self.process_timer();
                    }
                    Command::WorkerAvailable => {
                        self.backpressure(false);
                    }
                },
                Err(err) => match err {
                    sync_mpsc::TryRecvError::Empty => break,
                    sync_mpsc::TryRecvError::Disconnected => {
                        for (_, info) in self.sockets.iter_mut() {
                            let _ = self.poll.registry().deregister(&mut info.sock);
                        }
                        return false;
                    }
                },
            }
        }
        true
    }

    fn backpressure(&mut self, on: bool) {
        if self.backpressure {
            if !on {
                self.backpressure = false;
                for (token, info) in self.sockets.iter_mut() {
                    if info.timeout.is_some() {
                        // socket will re-register itself after timeout
                        continue;
                    }
                    if let Err(err) = self.poll.registry().register(
                        &mut info.sock,
                        mio::Token(token + DELTA),
                        mio::Interest::READABLE,
                    ) {
                        error!("Cannot resume socket accept process: {}", err);
                    } else {
                        info!("Accepting connections on {} has been resumed", info.addr);
                    }
                }
            }
        } else if on {
            self.backpressure = true;
            for (_, info) in self.sockets.iter_mut() {
                // disable err timeout
                if let None = info.timeout.take() {
                    trace!("Enabling backpressure for {}", info.addr);
                    let _ = self.poll.registry().deregister(&mut info.sock);
                }
            }
        }
    }

    fn accept_one(&mut self, mut msg: Connection) {
        trace!("Accepting connection: {:?}", msg.io);

        if self.backpressure {
            while !self.workers.is_empty() {
                match self.workers[self.next].send(msg) {
                    Ok(_) => (),
                    Err(tmp) => {
                        self.srv.worker_faulted(self.workers[self.next].idx);
                        msg = tmp;
                        self.workers.swap_remove(self.next);
                        if self.workers.is_empty() {
                            error!("No workers");
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
                            self.next = (self.next + 1) % self.workers.len();
                            return;
                        }
                        Err(tmp) => {
                            trace!("Worker failed while processing connection");
                            self.srv.worker_faulted(self.workers[self.next].idx);
                            msg = tmp;
                            self.workers.swap_remove(self.next);
                            if self.workers.is_empty() {
                                error!("No workers");
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
            trace!("No available workers, enable back-pressure");
            self.backpressure(true);
            self.accept_one(msg);
        }
    }

    fn accept(&mut self, token: usize) {
        loop {
            let msg = if let Some(info) = self.sockets.get_mut(token) {
                match info.sock.accept() {
                    Ok(Some(io)) => Connection {
                        io,
                        token: info.token,
                    },
                    Ok(None) => return,
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => return,
                    Err(ref e) if connection_error(e) => continue,
                    Err(e) => {
                        error!("Error accepting connection: {}", e);
                        if let Err(err) = self.poll.registry().deregister(&mut info.sock)
                        {
                            error!("Cannot deregister server socket {}", err);
                        }

                        // sleep after error
                        info.timeout = Some(Instant::now() + ERR_TIMEOUT);

                        let notify = self.notify.clone();
                        System::current().arbiter().spawn(Box::pin(async move {
                            sleep_until(Instant::now() + ERR_SLEEP_TIMEOUT).await;
                            notify.send(Command::Timer);
                        }));
                        return;
                    }
                }
            } else {
                return;
            };

            self.accept_one(msg);
        }
    }
}
