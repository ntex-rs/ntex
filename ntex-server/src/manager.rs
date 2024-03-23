use std::sync::atomic::{AtomicBool, Ordering};
use std::{cell::Cell, cell::RefCell, collections::VecDeque, rc::Rc, sync::Arc};

use async_channel::{unbounded, Receiver, Sender};
use ntex_rt::System;
use ntex_util::{future::join_all, time::sleep, time::Millis};

use crate::server::ServerShared;
use crate::signals::Signal;
use crate::{Server, ServerConfiguration, Worker, WorkerId, WorkerPool, WorkerStatus};

const STOP_DELAY: Millis = Millis(500);
const RESTART_DELAY: Millis = Millis(250);

#[derive(Clone)]
pub(crate) struct ServerManager<F: ServerConfiguration>(Rc<Inner<F>>);

#[derive(Debug)]
pub(crate) enum ServerCommand<T> {
    Item(T),
    Pause(oneshot::Sender<()>),
    Resume(oneshot::Sender<()>),
    Signal(Signal),
    Stop {
        graceful: bool,
        completion: Option<oneshot::Sender<()>>,
    },
    NotifyStopped(oneshot::Sender<()>),
    Worker(Update<T>),
}

#[derive(Debug)]
pub(crate) enum Update<T> {
    Available(Worker<T>),
    Unavailable(Worker<T>),
}

struct Inner<F: ServerConfiguration> {
    id: Cell<WorkerId>,
    factory: F,
    cfg: WorkerPool,
    shared: Arc<ServerShared>,
    stopping: Cell<bool>,
    stop_notify: RefCell<Vec<oneshot::Sender<()>>>,
    cmd: Sender<ServerCommand<F::Item>>,
}

impl<F: ServerConfiguration> ServerManager<F> {
    pub(crate) fn start(cfg: WorkerPool, factory: F) -> Server<F::Item> {
        log::info!("Starting {} workers", cfg.num);

        let (tx, rx) = unbounded();

        let no_signals = cfg.no_signals;
        let shared = Arc::new(ServerShared {
            paused: AtomicBool::new(true),
        });
        let mgr = ServerManager(Rc::new(Inner {
            cfg,
            factory,
            id: Cell::new(WorkerId::default()),
            shared: shared.clone(),
            stopping: Cell::new(false),
            stop_notify: RefCell::new(Vec::new()),
            cmd: tx.clone(),
        }));

        // handle cmd
        let _ = ntex_rt::spawn(handle_cmd(mgr.clone(), rx));

        // start workers
        for _ in 0..mgr.0.cfg.num {
            start_worker(mgr.clone());
        }

        let srv = Server::new(tx, shared);

        // handle signals
        if !no_signals {
            crate::signals::start(srv.clone());
        }

        srv
    }

    pub(crate) fn factory(&self) -> F {
        self.0.factory.clone()
    }

    pub(crate) fn next_id(&self) -> WorkerId {
        let mut id = self.0.id.get();
        let next_id = id.next();
        self.0.id.set(id);
        next_id
    }

    pub(crate) fn pause(&self) {
        self.0.shared.paused.store(true, Ordering::Release);
        self.0.factory.paused();
    }

    pub(crate) fn resume(&self) {
        self.0.shared.paused.store(false, Ordering::Release);
        self.0.factory.resumed();
    }

    fn available(&self, wrk: Worker<F::Item>) {
        let _ = self
            .0
            .cmd
            .try_send(ServerCommand::Worker(Update::Available(wrk)));
    }

    fn unavailable(&self, wrk: Worker<F::Item>) {
        let _ = self
            .0
            .cmd
            .try_send(ServerCommand::Worker(Update::Unavailable(wrk)));
    }

    fn add_stop_notify(&self, tx: oneshot::Sender<()>) {
        self.0.stop_notify.borrow_mut().push(tx);
    }

    fn stopping(&self) -> bool {
        self.0.stopping.get()
    }
}

fn start_worker<F: ServerConfiguration>(mgr: ServerManager<F>) {
    let _ = ntex_rt::spawn(async move {
        let id = mgr.next_id();
        let mut wrk = Worker::start(id, mgr.factory());

        loop {
            match wrk.status() {
                WorkerStatus::Available => mgr.available(wrk.clone()),
                WorkerStatus::Unavailable => mgr.unavailable(wrk.clone()),
                WorkerStatus::Failed => {
                    mgr.unavailable(wrk);
                    sleep(RESTART_DELAY).await;
                    if !mgr.stopping() {
                        wrk = Worker::start(id, mgr.factory());
                    } else {
                        return;
                    }
                }
            }
            wrk.wait_for_status().await;
        }
    });
}

struct HandleCmdState<F: ServerConfiguration> {
    next: usize,
    backlog: VecDeque<F::Item>,
    workers: Vec<Worker<F::Item>>,
    mgr: ServerManager<F>,
}

impl<F: ServerConfiguration> HandleCmdState<F> {
    fn new(mgr: ServerManager<F>) -> Self {
        Self {
            next: 0,
            backlog: VecDeque::new(),
            workers: Vec::with_capacity(mgr.0.cfg.num),
            mgr,
        }
    }

    fn process(&mut self, mut item: F::Item) {
        loop {
            if !self.workers.is_empty() {
                if self.next > self.workers.len() {
                    self.next = self.workers.len() - 1;
                }
                match self.workers[self.next].send(item) {
                    Ok(()) => {
                        self.next = (self.next + 1) % self.workers.len();
                        break;
                    }
                    Err(i) => {
                        if !self.mgr.0.stopping.get() {
                            log::trace!("Worker failed while processing item");
                        }
                        item = i;
                        self.workers.remove(self.next);
                    }
                }
            } else {
                if !self.mgr.0.stopping.get() {
                    log::error!("No workers");
                    self.backlog.push_back(item);
                    self.mgr.pause();
                }
                break;
            }
        }
    }

    fn update_workers(&mut self, upd: Update<F::Item>) {
        match upd {
            Update::Available(worker) => {
                self.workers.push(worker);
                if self.workers.len() == 1 {
                    self.mgr.resume();
                } else {
                    self.workers.sort();
                }
            }
            Update::Unavailable(worker) => {
                if let Ok(idx) = self.workers.binary_search(&worker) {
                    self.workers.remove(idx);
                }
                if self.workers.is_empty() {
                    self.mgr.pause();
                }
            }
        }
        // handle backlog
        if !self.backlog.is_empty() && !self.workers.is_empty() {
            while let Some(item) = self.backlog.pop_front() {
                // handle worker failure
                if let Err(item) = self.workers[0].send(item) {
                    self.backlog.push_back(item);
                    self.workers.remove(0);
                    break;
                }
            }
        }
    }

    async fn stop(&mut self, graceful: bool, completion: Option<oneshot::Sender<()>>) {
        self.mgr.0.stopping.set(true);

        // stop server
        self.mgr.0.factory.stop().await;

        // stop workers
        if !self.workers.is_empty() {
            let timeout = self.mgr.0.cfg.shutdown_timeout;

            if graceful && !timeout.is_zero() {
                let futs: Vec<_> = self
                    .workers
                    .iter()
                    .map(|worker| worker.stop(timeout))
                    .collect();

                let _ = join_all(futs).await;
            } else {
                self.workers.iter().for_each(|worker| {
                    let _ = worker.stop(Millis::ZERO);
                });
            }
        }

        // notify sender
        if let Some(tx) = completion {
            let _ = tx.send(());
        }

        let notify = std::mem::take(&mut *self.mgr.0.stop_notify.borrow_mut());
        for tx in notify {
            let _ = tx.send(());
        }

        // stop system if server was spawned
        if self.mgr.0.cfg.stop_runtime {
            sleep(STOP_DELAY).await;
            System::current().stop();
        }
    }
}

struct DropHandle<F: ServerConfiguration>(ServerManager<F>);

impl<T: ServerConfiguration> Drop for DropHandle<T> {
    fn drop(&mut self) {
        self.0 .0.stopping.set(true);
        self.0 .0.factory.terminate();
    }
}

async fn handle_cmd<F: ServerConfiguration>(
    mgr: ServerManager<F>,
    rx: Receiver<ServerCommand<F::Item>>,
) {
    let _drop_hnd = DropHandle(mgr.clone());
    let mut state = HandleCmdState::new(mgr);

    loop {
        let item = if let Ok(item) = rx.recv().await {
            item
        } else {
            return;
        };
        match item {
            ServerCommand::Item(item) => state.process(item),
            ServerCommand::Worker(upd) => state.update_workers(upd),
            ServerCommand::Pause(tx) => {
                state.mgr.pause();
                let _ = tx.send(());
            }
            ServerCommand::Resume(tx) => {
                state.mgr.resume();
                let _ = tx.send(());
            }
            ServerCommand::NotifyStopped(tx) => state.mgr.add_stop_notify(tx),
            ServerCommand::Stop {
                graceful,
                completion,
            } => {
                state.stop(graceful, completion).await;
                return;
            }
            ServerCommand::Signal(sig) => {
                // Signals support
                // Handle `SIGINT`, `SIGTERM`, `SIGQUIT` signals and stop ntex system
                match sig {
                    Signal::Int => {
                        log::info!("SIGINT received, exiting");
                        state.stop(false, None).await;
                        return;
                    }
                    Signal::Term => {
                        log::info!("SIGTERM received, stopping");
                        state.stop(true, None).await;
                        return;
                    }
                    Signal::Quit => {
                        log::info!("SIGQUIT received, exiting");
                        state.stop(false, None).await;
                        return;
                    }
                    _ => (),
                }
            }
        }
    }
}
