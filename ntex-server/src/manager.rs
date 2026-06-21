use std::sync::atomic::{AtomicBool, Ordering};
use std::{cell::Cell, cell::RefCell, collections::VecDeque, rc::Rc, sync::Arc};

use async_channel::{Receiver, Sender, TrySendError, unbounded};
use core_affinity::CoreId;
use ntex_rt::{System, signals::Signal};
use ntex_util::future::join_all;
use ntex_util::time::{Millis, sleep, timeout};

use crate::server::ServerShared;
use crate::{Server, ServerConfiguration, Worker, WorkerId, WorkerPool, WorkerStatus};

const STOP_DELAY: Millis = Millis(250);
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
        log::info!("Starting {:?} {} workers", cfg.name, cfg.num);

        let (tx, rx) = unbounded();

        let affinity = cfg.affinity;
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
        ntex_rt::spawn(handle_cmd(mgr.clone(), rx));

        // Retrieve the IDs of all active CPU cores.
        let mut cores = if affinity {
            core_affinity::get_core_ids().unwrap_or_default()
        } else {
            Vec::new()
        };

        // start workers
        for _ in 0..mgr.0.cfg.num {
            start_worker(mgr.clone(), cores.pop());
        }

        let srv = Server::new(tx, shared);

        // handle signals
        if no_signals {
            System::current().disable_signals();
        } else {
            System::current().enable_signals();

            let srv2 = srv.clone();
            ntex_rt::spawn(async move {
                while let Ok(sigs) = ntex_rt::signals::signal().await {
                    for sig in sigs.as_ref() {
                        srv2.signal(*sig);
                    }
                }
            });
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
        log::trace!("Manager {:?}: pause requested", self.0.cfg.name);
        self.0.shared.paused.store(true, Ordering::Release);
        self.0.factory.paused();
    }

    pub(crate) fn resume(&self) {
        log::trace!("Manager {:?}: resume requested", self.0.cfg.name);
        self.0.shared.paused.store(false, Ordering::Release);
        self.0.factory.resumed();
    }

    fn available(&self, wrk: Worker<F::Item>) {
        let name = wrk.name().to_string();
        match self
            .0
            .cmd
            .try_send(ServerCommand::Worker(Update::Available(wrk)))
        {
            Ok(()) => log::trace!(
                "Manager {:?}: enqueued Available for {name:?}",
                self.0.cfg.name
            ),
            Err(TrySendError::Full(_)) => {
                log::error!(
                    "Manager {:?}: command queue full for Available {name:?}",
                    self.0.cfg.name
                )
            }
            Err(TrySendError::Closed(_)) => {
                log::error!(
                    "Manager {:?}: command queue closed for Available {name:?}",
                    self.0.cfg.name
                )
            }
        }
    }

    fn unavailable(&self, wrk: Worker<F::Item>) {
        let name = wrk.name().to_string();
        match self
            .0
            .cmd
            .try_send(ServerCommand::Worker(Update::Unavailable(wrk)))
        {
            Ok(()) => {
                log::trace!(
                    "Manager {:?}: enqueued Unavailable for {name:?}",
                    self.0.cfg.name
                )
            }
            Err(TrySendError::Full(_)) => {
                log::error!(
                    "Manager {:?}: command queue full for Unavailable {name:?}",
                    self.0.cfg.name
                )
            }
            Err(TrySendError::Closed(_)) => {
                log::error!(
                    "Manager {:?}: command queue closed for Unavailable {name:?}",
                    self.0.cfg.name
                )
            }
        }
    }

    fn add_stop_notify(&self, tx: oneshot::Sender<()>) {
        self.0.stop_notify.borrow_mut().push(tx);
    }

    fn stopping(&self) -> bool {
        self.0.stopping.get()
    }
}

fn start_worker<F: ServerConfiguration>(mgr: ServerManager<F>, cid: Option<CoreId>) {
    ntex_rt::spawn(async move {
        let id = mgr.next_id();
        let name = format!("{}:worker:{}", mgr.0.cfg.name, id.0);
        let mut wrk = Worker::start(name.clone(), mgr.factory(), cid);

        loop {
            let status = wrk.status();
            log::trace!(
                "Manager {:?}: observed {name:?} status {status:?}",
                mgr.0.cfg.name
            );
            match status {
                WorkerStatus::Available => mgr.available(wrk.clone()),
                WorkerStatus::Unavailable => mgr.unavailable(wrk.clone()),
                WorkerStatus::Failed => {
                    log::error!("Manager {:?}: observed {name:?} failed", mgr.0.cfg.name);
                    mgr.unavailable(wrk);
                    sleep(RESTART_DELAY).await;
                    if mgr.stopping() {
                        return;
                    }
                    wrk = Worker::start(name.clone(), mgr.factory(), cid);
                }
            }
            let status = wrk.wait_for_status().await;
            log::trace!(
                "Manager {:?}: wait_for_status woke for {name:?} with {status:?}",
                mgr.0.cfg.name
            );
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
            if self.workers.is_empty() {
                if !self.mgr.0.stopping.get() {
                    log::error!(
                        "Manager {:?}: no workers, backlog before push {}",
                        self.mgr.0.cfg.name,
                        self.backlog.len()
                    );
                    self.backlog.push_back(item);
                    self.mgr.pause();
                }
                break;
            }
            if self.next >= self.workers.len() {
                self.next = self.workers.len() - 1;
            }
            match self.workers[self.next].send(item) {
                Ok(()) => {
                    log::trace!(
                        "Manager {:?}: dispatched item to worker {:?}, workers={}, backlog={}",
                        self.mgr.0.cfg.name,
                        self.workers[self.next].name(),
                        self.workers.len(),
                        self.backlog.len()
                    );
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
        }
    }

    fn update_workers(&mut self, upd: Update<F::Item>) {
        match upd {
            Update::Available(worker) => {
                let name = worker.name().to_string();
                let before = self.workers.len();
                self.workers.push(worker);
                self.workers.sort();
                log::trace!(
                    "Manager {:?}: worker {name:?} Available, workers {before}->{}, backlog={}",
                    self.mgr.0.cfg.name,
                    self.workers.len(),
                    self.backlog.len()
                );
                if self.workers.len() == 1 {
                    log::trace!(
                        "Manager {:?}: first available worker {name:?}, resuming",
                        self.mgr.0.cfg.name
                    );
                    self.mgr.resume();
                }
            }
            Update::Unavailable(worker) => {
                let name = worker.name().to_string();
                let before = self.workers.len();
                if let Ok(idx) = self.workers.binary_search(&worker) {
                    self.workers.remove(idx);
                }
                log::trace!(
                    "Manager {:?}: worker {name:?} Unavailable, workers {before}->{}, backlog={}",
                    self.mgr.0.cfg.name,
                    self.workers.len(),
                    self.backlog.len()
                );
                if self.workers.is_empty() {
                    log::trace!(
                        "Manager {:?}: no available workers after {name:?}, pausing",
                        self.mgr.0.cfg.name
                    );
                    self.mgr.pause();
                }
            }
        }
        // handle backlog
        if !self.backlog.is_empty() && !self.workers.is_empty() {
            while let Some(item) = self.backlog.pop_front() {
                // handle worker failure
                if let Err(item) = self.workers[0].send(item) {
                    log::trace!(
                        "Manager {:?}: backlog dispatch to {:?} failed",
                        self.mgr.0.cfg.name,
                        self.workers[0].name()
                    );
                    self.backlog.push_back(item);
                    self.workers.remove(0);
                    if self.workers.is_empty() {
                        self.mgr.pause();
                    }
                    break;
                }
            }
        }
    }

    async fn stop(&mut self, graceful: bool, completion: Option<oneshot::Sender<()>>) {
        log::info!(
            "Stopping {:?} server, graceful({graceful})",
            self.mgr.0.cfg.name
        );

        self.mgr.0.stopping.set(true);

        // stop server
        self.mgr.0.factory.stop().await;

        // stop workers
        if !self.workers.is_empty() {
            let to = self.mgr.0.cfg.shutdown_timeout;

            if graceful && !to.is_zero() {
                let futs: Vec<_> =
                    self.workers.iter().map(|worker| worker.stop(to)).collect();

                let _ = timeout(to, join_all(futs)).await;
            } else {
                self.workers.iter().for_each(|worker| {
                    worker.stop(Millis::ZERO);
                });
            }
        }

        if !System::current().testing() {
            sleep(STOP_DELAY).await;
        }
        log::info!("All worker are stopped in {:?} server", self.mgr.0.cfg.name);

        // notify Server instance
        let notify = std::mem::take(&mut *self.mgr.0.stop_notify.borrow_mut());
        for tx in notify {
            let _ = tx.send(());
        }

        // notify sender
        log::trace!(
            "Notify caller {} of {:?} server",
            completion.is_some(),
            self.mgr.0.cfg.name
        );
        if let Some(tx) = completion {
            let _ = tx.send(());
        }

        // stop system
        if self.mgr.0.cfg.stop_runtime {
            log::trace!("Stopping ntex system, {:?} server", self.mgr.0.cfg.name);
            System::current().stop();
        }

        log::info!("Server {:?} has been stopped", self.mgr.0.cfg.name);
    }
}

struct DropHandle<F: ServerConfiguration>(ServerManager<F>);

impl<T: ServerConfiguration> Drop for DropHandle<T> {
    fn drop(&mut self) {
        self.0.0.stopping.set(true);
        self.0.0.factory.terminate();
    }
}

async fn handle_cmd<F: ServerConfiguration>(
    mgr: ServerManager<F>,
    rx: Receiver<ServerCommand<F::Item>>,
) {
    let _drop_hnd = DropHandle(mgr.clone());
    let mut state = HandleCmdState::new(mgr);

    loop {
        let Ok(item) = rx.recv().await else {
            log::trace!("Manager command loop receiver closed");
            return;
        };
        match item {
            ServerCommand::Item(item) => state.process(item),
            ServerCommand::Worker(upd) => {
                log::trace!("Manager {:?}: received worker update", state.mgr.0.cfg.name);
                state.update_workers(upd);
            }
            ServerCommand::Pause(tx) => {
                log::trace!("Manager {:?}: received Pause command", state.mgr.0.cfg.name);
                state.mgr.pause();
                let _ = tx.send(());
            }
            ServerCommand::Resume(tx) => {
                log::trace!(
                    "Manager {:?}: received Resume command",
                    state.mgr.0.cfg.name
                );
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
                    Signal::Hup => (),
                }
            }
        }
    }
}
