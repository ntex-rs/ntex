use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError};
use std::{cell::Cell, cell::RefCell, collections::VecDeque, fmt, pin::pin, rc::Rc};

use async_channel::{Receiver, Sender, unbounded};
use ntex_rt::{System, signals::PanicSource, signals::Signal};
use ntex_util::future::{Either, join_all, select};
use ntex_util::time::{Millis, interval, sleep, timeout};

use crate::affinity::{self, Placement};
use crate::{Server, ServerConfiguration, ServerStatus};
use crate::{Worker, WorkerId, WorkerPool, WorkerStatus};
use crate::{balance::Balancer, metrics::Metrics, server::ServerShared};

const STOP_DELAY: Millis = Millis(250);
const RESTART_DELAY: Millis = Millis(250);
/// Interval of the check for items queued at blocked workers.
const RESCHEDULE_INTERVAL: Millis = Millis(50);

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
    router: Arc<Routing<F::Item>>,
    /// All running workers, including unavailable ones.
    workers: RefCell<Vec<Worker<F::Item>>>,
    /// Paused with [`Server::pause`].
    paused: Cell<bool>,
    /// A worker can receive items and no item waits in the backlog.
    available: Cell<bool>,
    /// A worker has been available.
    started: Cell<bool>,
    stopping: Cell<bool>,
    stopped: Cell<bool>,
    stop_notify: RefCell<Vec<oneshot::Sender<()>>>,
    cmd: Sender<ServerCommand<F::Item>>,
}

/// Sends items to the workers.
///
/// Shared by the command loop, which adds and removes workers, and the
/// [`Server`] handles, which send items to the workers on the submitting
/// thread, e.g. the accept thread.
pub(crate) struct Routing<T> {
    router: Mutex<Router<T>>,
    /// Called without holding the router's lock, so it can take time or
    /// submit items to the server.
    incoming_cpu: Mutex<IncomingCpu<T>>,
}

/// Workers that receive new items, and their load balancer.
pub(crate) struct Router<T> {
    workers: Vec<Worker<T>>,
    balancer: Balancer,
    /// The server is stopping, items are not sent to workers.
    closed: bool,
}

/// See [`ServerConfiguration::incoming_cpu`].
type IncomingCpu<T> = Box<dyn Fn(&T) -> Option<usize> + Send>;

/// An item that was not sent to a worker.
#[derive(Debug, PartialEq)]
pub(crate) enum Rejected<T> {
    /// No worker is available.
    NoWorker(T),
    /// The server is stopping.
    Closed(T),
}

impl<T> Rejected<T> {
    fn into_inner(self) -> T {
        match self {
            Rejected::NoWorker(item) | Rejected::Closed(item) => item,
        }
    }
}

impl<T> Routing<T> {
    fn new(balancer: Balancer, capacity: usize, incoming_cpu: IncomingCpu<T>) -> Self {
        Routing {
            router: Mutex::new(Router {
                workers: Vec::with_capacity(capacity),
                balancer,
                closed: false,
            }),
            incoming_cpu: Mutex::new(incoming_cpu),
        }
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, Router<T>> {
        self.router.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Sends `item` to a worker, returns it if the server is stopping or no
    /// worker is available.
    pub(crate) fn dispatch(&self, item: T, metrics: &Metrics) -> Result<(), Rejected<T>> {
        let uses_cpu = {
            let mut r = self.lock();
            if r.closed {
                return Err(Rejected::Closed(item));
            } else if r.workers.is_empty() {
                return Err(Rejected::NoWorker(item));
            }
            let r = &mut *r;
            r.balancer.uses_incoming_cpu(&r.workers)
        };
        // the callback is busy if it submits the item to this server, or
        // another thread runs it, the item is sent without its cpu
        let cpu = if uses_cpu {
            match self.incoming_cpu.try_lock() {
                Ok(f) => f(&item),
                Err(TryLockError::Poisoned(f)) => f.into_inner()(&item),
                Err(TryLockError::WouldBlock) => None,
            }
        } else {
            None
        };
        self.lock().dispatch(item, cpu, metrics)
    }

    /// Sends a rescheduled item to a worker, without its incoming CPU.
    fn redispatch(&self, item: T, metrics: &Metrics) -> Result<(), Rejected<T>> {
        self.lock().dispatch(item, None, metrics)
    }
}

impl<T> Router<T> {
    fn dispatch(
        &mut self,
        mut item: T,
        cpu: Option<usize>,
        metrics: &Metrics,
    ) -> Result<(), Rejected<T>> {
        if self.closed {
            return Err(Rejected::Closed(item));
        }
        while !self.workers.is_empty() {
            let (idx, local) = self.balancer.select(&self.workers, cpu);
            match self.workers[idx].send(item) {
                Ok(()) => {
                    self.balancer.sent(idx, self.workers.len());
                    if let Some(local) = local {
                        metrics.incoming_cpu(local);
                    }
                    return Ok(());
                }
                Err(i) => {
                    log::trace!("Worker failed while processing item");
                    item = i;
                    self.workers.remove(idx);
                    self.balancer.remove(idx);
                }
            }
        }
        Err(Rejected::NoWorker(item))
    }
}

impl<T> fmt::Debug for Routing<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let r = self.lock();
        f.debug_struct("Routing")
            .field("workers", &r.workers.len())
            .field("closed", &r.closed)
            .finish_non_exhaustive()
    }
}

impl<F: ServerConfiguration> ServerManager<F> {
    pub(crate) fn start(cfg: WorkerPool, factory: F) -> Server<F::Item> {
        log::info!("Starting {:?} {} workers", cfg.name, cfg.num);

        let (tx, rx) = unbounded();

        let affinity = cfg.affinity;
        let no_signals = cfg.no_signals;
        let shared = Arc::new(ServerShared::new(Metrics::new(cfg.num)));
        let f = factory.clone();
        let router = Arc::new(Routing::new(
            Balancer::new(cfg.balancing),
            cfg.num,
            Box::new(move |item| f.incoming_cpu(item)),
        ));
        let mgr = ServerManager(Rc::new(Inner {
            workers: RefCell::new(Vec::with_capacity(cfg.num)),
            cfg,
            factory,
            id: Cell::new(WorkerId::default()),
            shared: shared.clone(),
            router: router.clone(),
            paused: Cell::new(false),
            available: Cell::new(false),
            started: Cell::new(false),
            stopping: Cell::new(false),
            stopped: Cell::new(false),
            stop_notify: RefCell::new(Vec::new()),
            cmd: tx.clone(),
        }));

        // handle cmd
        ntex_rt::spawn_detached(handle_cmd(mgr.clone(), rx));

        // CPU placement of each worker
        let plan = affinity::plan(affinity);

        // start workers
        for idx in 0..mgr.0.cfg.num {
            let placement = (!plan.is_empty()).then(|| plan[idx % plan.len()].clone());
            start_worker(mgr.clone(), placement);
        }

        let srv = Server::new(tx, shared, router);

        // handle signals
        if no_signals {
            System::current().disable_signals();
        } else {
            System::current().enable_signals();

            // the task ends when the server stops, so it does not keep the
            // server's resources alive
            let srv2 = srv.clone();
            ntex_rt::spawn_detached(async move {
                let mut stopped =
                    pin!(async { while srv2.status_changed().await != ServerStatus::Stopped {} });
                while let Either::Left(Ok(sigs)) =
                    select(ntex_rt::signals::signal(), stopped.as_mut()).await
                {
                    for sig in sigs.as_ref() {
                        srv2.signal(sig.clone());
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

    fn set_paused(&self, paused: bool) {
        self.0.paused.set(paused);
        self.update_status();
    }

    fn set_available(&self, available: bool) {
        self.0.available.set(available);
        if available {
            self.0.started.set(true);
        }
        self.update_status();
    }

    /// Updates the server status, the configuration is paused or resumed
    /// when the server stops or becomes ready.
    fn update_status(&self) {
        let inner = &self.0;
        let status = if inner.stopped.get() {
            ServerStatus::Stopped
        } else if inner.stopping.get() {
            ServerStatus::Stopping
        } else if inner.paused.get() {
            ServerStatus::Paused
        } else if inner.available.get() {
            ServerStatus::Ready
        } else if inner.started.get() {
            ServerStatus::Unavailable
        } else {
            ServerStatus::Starting
        };

        let prev = inner.shared.status();
        if prev != status {
            log::debug!("{:?} server status: {status:?}", inner.cfg.name);
            // the status is set first, items are accepted once the
            // configuration resumes
            inner.shared.set_status(status);
            // the configuration is stopped instead of paused
            if !inner.stopping.get() {
                if status == ServerStatus::Ready {
                    inner.factory.resume();
                } else if prev == ServerStatus::Ready {
                    inner.factory.pause();
                }
            }
        }
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

    /// Registers a started or restarted worker.
    fn register(&self, wrk: &Worker<F::Item>) {
        let mut workers = self.0.workers.borrow_mut();
        if let Some(w) = workers.iter_mut().find(|w| *w == wrk) {
            *w = wrk.clone();
        } else {
            workers.push(wrk.clone());
        }
    }

    fn unregister(&self, wrk: &Worker<F::Item>) {
        self.0.workers.borrow_mut().retain(|w| w != wrk);
    }

    fn add_stop_notify(&self, tx: oneshot::Sender<()>) {
        if self.0.stopped.get() {
            let _ = tx.send(());
        } else {
            self.0.stop_notify.borrow_mut().push(tx);
        }
    }

    fn stopping(&self) -> bool {
        self.0.stopping.get()
    }
}

fn start_worker<F: ServerConfiguration>(mgr: ServerManager<F>, placement: Option<Placement>) {
    let id = mgr.next_id();
    let name = format!("{}:worker:{}", mgr.0.cfg.name, id.0);
    let slot = mgr.0.shared.metrics.worker(id);
    let mut wrk = Worker::start_with(name.clone(), mgr.factory(), placement.clone(), slot.clone());
    // registered before any command is handled, the server stops every
    // running worker, not only the available ones
    mgr.register(&wrk);

    ntex_rt::spawn_detached(async move {
        loop {
            match wrk.status() {
                WorkerStatus::Failed => {
                    if mgr.stopping() {
                        break;
                    }
                    if mgr.0.cfg.stop_on_panic {
                        log::info!(
                            "Worker failed. `stop-on-panic` is enabled; shutting down the server."
                        );
                        let graceful = mgr.0.cfg.graceful_shutdown;
                        HandleCmdState::new(mgr.clone()).stop(graceful, None).await;
                        break;
                    }
                    mgr.unavailable(wrk.clone());
                    sleep(RESTART_DELAY).await;
                    if mgr.stopping() {
                        break;
                    }
                    if let Some(ref slot) = slot {
                        slot.restart();
                    }
                    wrk = Worker::start_with(
                        name.clone(),
                        mgr.factory(),
                        placement.clone(),
                        slot.clone(),
                    );
                    mgr.register(&wrk);
                }
                // the server is stopping, the worker is not routed
                _ if mgr.stopping() => (),
                WorkerStatus::Available => mgr.available(wrk.clone()),
                WorkerStatus::Unavailable => mgr.unavailable(wrk.clone()),
            }
            wrk.wait_for_status().await;
        }
        mgr.unregister(&wrk);
    });
}

struct HandleCmdState<F: ServerConfiguration> {
    /// Items received while no worker was available, and whether the item
    /// was rescheduled from a worker.
    backlog: VecDeque<(F::Item, bool)>,
    mgr: ServerManager<F>,
}

impl<F: ServerConfiguration> HandleCmdState<F> {
    fn new(mgr: ServerManager<F>) -> Self {
        Self {
            backlog: VecDeque::new(),
            mgr,
        }
    }

    fn process(&mut self, item: F::Item) {
        // the item is dropped if the server is stopping
        if let Err(Rejected::NoWorker(item)) =
            self.mgr.0.router.dispatch(item, &self.mgr.0.shared.metrics)
        {
            log::error!("No workers");
            self.backlog.push_back((item, false));
            self.mgr.set_available(false);
        }
    }

    /// Moves the items queued at blocked workers to other workers.
    fn reschedule_blocked(&mut self) {
        let metrics = &self.mgr.0.shared.metrics;
        let routing = self.mgr.0.router.clone();
        let mut router = routing.lock();
        let r = &mut *router;
        // only if some workers are not blocked
        if r.closed || !r.balancer.sample(&r.workers) {
            return;
        }
        let mut items = Vec::new();
        for (idx, wrk) in r.workers.iter().enumerate() {
            if r.balancer.is_blocked(idx) {
                let taken = wrk.take_queued(false);
                if !taken.is_empty() {
                    log::debug!(
                        "Rescheduling {} items of blocked worker {:?}",
                        taken.len(),
                        wrk.name()
                    );
                    items.extend(taken);
                }
            }
        }
        metrics.rescheduled(items.len() as u64);

        // the server is unavailable while the backlog is not empty
        for item in items {
            if !self.backlog.is_empty() {
                self.backlog.push_back((item, true));
            } else if let Err(Rejected::NoWorker(item)) = r.dispatch(item, None, metrics) {
                self.backlog.push_back((item, true));
            }
        }
        drop(router);
        if !self.backlog.is_empty() {
            self.mgr.set_available(false);
        }
    }

    fn update_workers(&mut self, upd: Update<F::Item>) {
        let routing = self.mgr.0.router.clone();
        let mut router = routing.lock();
        let r = &mut *router;
        match upd {
            Update::Available(worker) => match r.workers.binary_search(&worker) {
                Ok(idx) => {
                    // a restarted worker runs on a new arbiter
                    if r.workers[idx].arbiter() != worker.arbiter() {
                        r.balancer.reset(idx);
                    }
                    r.workers[idx] = worker;
                }
                Err(idx) => {
                    r.balancer.unpark(idx, worker.name(), worker.arbiter().id());
                    r.workers.insert(idx, worker);
                }
            },
            Update::Unavailable(worker) => {
                if let Ok(idx) = r.workers.binary_search(&worker) {
                    // the worker keeps its runtime and connections, its
                    // load history stays valid
                    let wrk = r.workers.remove(idx);
                    r.balancer.park(idx, wrk.name(), wrk.arbiter().id());
                }
                // items the worker has not received are sent to other
                // workers, the update is stale if the worker is available
                let status = worker.status();
                if status != WorkerStatus::Available {
                    let items = worker.take_queued(status == WorkerStatus::Failed);
                    if !items.is_empty() {
                        log::debug!(
                            "Rescheduling {} items of {status:?} worker {:?}",
                            items.len(),
                            worker.name()
                        );
                        self.mgr.0.shared.metrics.rescheduled(items.len() as u64);
                        self.backlog
                            .extend(items.into_iter().map(|item| (item, true)));
                    }
                }
            }
        }
        drop(router);

        // handle backlog, the server is unavailable while it is not empty, so
        // new items do not overtake it
        let metrics = &self.mgr.0.shared.metrics;
        while let Some((item, rescheduled)) = self.backlog.pop_front() {
            let res = if rescheduled {
                routing.redispatch(item, metrics)
            } else {
                routing.dispatch(item, metrics)
            };
            if let Err(item) = res {
                self.backlog.push_front((item.into_inner(), rescheduled));
                break;
            }
        }
        let available = self.backlog.is_empty() && !routing.lock().workers.is_empty();
        self.mgr.set_available(available);
    }

    async fn stop(&mut self, graceful: bool, completion: Option<oneshot::Sender<()>>) {
        // another stop is in progress or has completed
        if self.mgr.stopping() {
            if let Some(tx) = completion {
                self.mgr.add_stop_notify(tx);
            }
            return;
        }

        log::info!(
            "Stopping {:?} server, graceful({graceful})",
            self.mgr.0.cfg.name
        );

        self.mgr.0.stopping.set(true);
        self.mgr.update_status();

        // new items are not sent to the workers
        {
            let mut router = self.mgr.0.router.lock();
            router.closed = true;
            router.workers.clear();
        }
        // unavailable and initializing workers are stopped as well
        let workers = self.mgr.0.workers.borrow().clone();

        // stop server
        self.mgr.0.factory.stop().await;

        // stop workers
        if !workers.is_empty() {
            let to = self.mgr.0.cfg.graceful_shutdown_timeout;
            if graceful && !to.is_zero() {
                let futs: Vec<_> = workers.iter().map(|worker| worker.stop(to)).collect();

                let _ = timeout(to, join_all(futs)).await;
            } else {
                for worker in &workers {
                    worker.stop(Millis::ZERO);
                }
            }
        }

        if !System::current().testing() {
            sleep(STOP_DELAY).await;
        }
        log::info!("All worker are stopped in {:?} server", self.mgr.0.cfg.name);

        // notify Server instance
        self.mgr.0.stopped.set(true);
        self.mgr.update_status();
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

        // reject new commands, the command loop handles queued ones
        self.mgr.0.cmd.close();

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
        self.0.0.stopped.set(true);
        self.0.update_status();
        self.0.0.factory.terminate();
    }
}

async fn handle_cmd<F: ServerConfiguration>(
    mgr: ServerManager<F>,
    rx: Receiver<ServerCommand<F::Item>>,
) {
    let _drop_hnd = DropHandle(mgr.clone());
    // queued items are moved from blocked workers to other workers
    let check = (mgr.0.cfg.num >= 2).then(|| interval(RESCHEDULE_INTERVAL));
    let mut state = HandleCmdState::new(mgr);

    loop {
        let item = if let Some(ref check) = check {
            match select(rx.recv(), check.tick()).await {
                Either::Left(item) => item,
                Either::Right(()) => {
                    if !state.mgr.stopping() {
                        state.reschedule_blocked();
                    }
                    continue;
                }
            }
        } else {
            rx.recv().await
        };
        let Ok(item) = item else {
            return;
        };

        // the server is stopping, reply to commands that wait for a response
        if state.mgr.stopping() {
            match item {
                ServerCommand::Stop {
                    completion: Some(tx),
                    ..
                }
                | ServerCommand::NotifyStopped(tx) => state.mgr.add_stop_notify(tx),
                ServerCommand::Pause(tx) | ServerCommand::Resume(tx) => {
                    let _ = tx.send(());
                }
                _ => (),
            }
            continue;
        }

        match item {
            ServerCommand::Item(item) => state.process(item),
            ServerCommand::Worker(upd) => state.update_workers(upd),
            ServerCommand::Pause(tx) => {
                state.mgr.set_paused(true);
                let _ = tx.send(());
            }
            ServerCommand::Resume(tx) => {
                state.mgr.set_paused(false);
                let _ = tx.send(());
            }
            ServerCommand::NotifyStopped(tx) => state.mgr.add_stop_notify(tx),
            ServerCommand::Stop {
                graceful,
                completion,
            } => {
                state.stop(graceful, completion).await;
            }
            ServerCommand::Signal(sig) => {
                // Signals support
                // Handle `SIGINT`, `SIGTERM`, `SIGQUIT` signals and stop ntex system
                match sig {
                    Signal::Int => {
                        log::info!("SIGINT received, exiting");
                        state.stop(false, None).await;
                    }
                    Signal::Term => {
                        log::info!("SIGTERM received, stopping");
                        state.stop(true, None).await;
                    }
                    Signal::Quit => {
                        log::info!("SIGQUIT received, exiting");
                        state.stop(state.mgr.0.cfg.graceful_shutdown, None).await;
                    }
                    Signal::Panic(PanicSource::Sig(s)) => {
                        log::info!("{s} received, exiting");
                        state.stop(state.mgr.0.cfg.graceful_shutdown, None).await;
                    }
                    Signal::Panic(PanicSource::App(s, _)) => {
                        log::info!("Application paniced exiting, {s}");
                        state.stop(state.mgr.0.cfg.graceful_shutdown, None).await;
                    }
                    Signal::Hup => (),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unused_async_trait_impl)]
    use std::sync::{OnceLock, Weak, atomic::AtomicUsize, atomic::Ordering, mpsc};
    use std::time::Duration;

    use ntex_service::{Ctx, Service};

    use super::*;
    use crate::LoadBalancing;

    #[derive(Clone)]
    struct Cfg;

    struct Srv;

    impl Service<(), usize> for Srv {
        type Res = ();
        type Error = ();

        async fn call(&self, _: usize, _: Ctx<'_, Self, ()>) -> Result<(), ()> {
            Ok(())
        }
    }

    impl ServerConfiguration for Cfg {
        type Item = usize;
        type Service = Srv;

        async fn create(&self) -> std::io::Result<Srv> {
            Ok(Srv)
        }
    }

    /// The incoming CPU callback runs without the router's lock, an item it
    /// submits to the same server is sent without its CPU.
    #[ntex::test]
    async fn incoming_cpu_reentrant_dispatch() {
        let mut wrk = Worker::start("test".into(), Cfg, None);
        assert_eq!(wrk.wait_for_status().await, WorkerStatus::Available);
        wrk.set_cpus(&[0]);

        let routing: Arc<OnceLock<Weak<Routing<usize>>>> = Arc::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let (r2, calls2) = (routing.clone(), calls.clone());
        let callback: IncomingCpu<usize> = Box::new(move |item| {
            calls2.fetch_add(1, Ordering::SeqCst);
            if *item == 1 {
                let r = r2.get().unwrap().upgrade().unwrap();
                assert_eq!(r.dispatch(2, &Metrics::new(1)), Ok(()));
            }
            Some(0)
        });
        let mut balancer = Balancer::new(LoadBalancing::IncomingCpu);
        balancer.insert(0);
        let r = Arc::new(Routing::new(balancer, 1, callback));
        r.lock().workers.push(wrk.clone());
        routing.set(Arc::downgrade(&r)).unwrap();

        // a deadlocked dispatch blocks its thread
        let (tx, rx) = mpsc::channel();
        let r2 = r.clone();
        std::thread::spawn(move || {
            let _ = tx.send(r2.dispatch(1, &Metrics::new(1)));
        });
        let res = rx.recv_timeout(Duration::from_secs(5));
        assert_eq!(res, Ok(Ok(())), "dispatch deadlocked");
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        wrk.stop(Millis::ZERO).await;
    }

    /// An item submitted after the server passed the status check and
    /// closed its router is returned, not queued.
    #[test]
    fn process_closed_router() {
        let shared = Arc::new(ServerShared::new(Metrics::new(1)));
        shared.set_status(ServerStatus::Ready);
        let callback: IncomingCpu<usize> = Box::new(|_| None);
        let r = Arc::new(Routing::new(
            Balancer::new(LoadBalancing::IncomingCpu),
            1,
            callback,
        ));
        let (tx, rx) = unbounded();
        let srv = Server::new(tx, shared, r.clone());

        // no worker, the command loop queues the item
        assert_eq!(srv.process(1), Ok(()));
        assert!(matches!(rx.try_recv(), Ok(ServerCommand::Item(1))));

        r.lock().closed = true;
        assert_eq!(srv.process(2), Err(2));
        assert!(rx.try_recv().is_err());
    }
}
