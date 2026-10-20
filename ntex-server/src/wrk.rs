use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll, ready};
use std::{cmp, future::Future, future::poll_fn, hash, pin::Pin};

use async_channel::{Receiver, Sender, TrySendError, unbounded};
use event_listener::Event;

use ntex_rt::{Arbiter, spawn};
use ntex_service::Pipeline;
use ntex_util::future::{Either, Stream, select, stream_recv};
use ntex_util::time::{Millis, sleep, timeout_checked};

use crate::{ServerConfiguration, affinity::Placement, metrics::WorkerSlot};

const STOP_TIMEOUT: Millis = Millis(3000);

#[derive(Debug)]
/// Shutdown worker command.
struct Shutdown {
    timeout: Millis,
    result: oneshot::Sender<bool>,
}

#[derive(Copy, Clone, Default, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
/// Worker status.
pub enum WorkerStatus {
    /// The worker is ready to accept an item.
    Available,
    /// The worker is temporarily unable to accept an item.
    #[default]
    Unavailable,
    /// The worker has stopped.
    ///
    /// Reported when the worker panics, its service cannot be created, or
    /// it finishes a regular stop.
    Failed,
}

#[derive(Debug)]
/// Server worker.
///
/// A worker runs one service instance on its own arbiter thread. It receives
/// items through an unbounded channel. Clones refer to the same worker.
pub struct Worker<T> {
    name: String,
    reqs: Sender<T>,
    /// Takes back items the worker has not received yet.
    queue: Receiver<T>,
    stop: Sender<Shutdown>,
    avail: WorkerAvailability,
    arbiter: Arbiter,
    metrics: Option<Arc<WorkerSlot>>,
    /// Set once the placement is applied.
    cpus: Arc<OnceLock<Box<[usize]>>>,
}

#[derive(Debug)]
/// Future returned when stopping a worker.
///
/// Resolves to `true` if the worker's service shut down within the timeout,
/// or if the worker was already gone. Resolves to `false` if the timeout
/// expired, or if the worker was stopped before its service was created.
pub struct WorkerStop(oneshot::AsyncReceiver<bool>);

impl<T> Worker<T> {
    /// Starts a worker on a new arbiter thread.
    ///
    /// Returns immediately. The worker reports
    /// [`Unavailable`](WorkerStatus::Unavailable) until its service is
    /// created. If `cpu` is set, the worker thread is pinned to the logical
    /// CPU with that index. On Windows, CPUs are numbered across processor
    /// groups, the CPUs of group 1 follow the CPUs of group 0. Pinning is
    /// supported on Windows, Linux and Android.
    pub fn start<F>(name: String, cfg: F, cpu: Option<usize>) -> Worker<T>
    where
        T: Send + 'static,
        F: ServerConfiguration<Item = T>,
    {
        Self::start_with(name, cfg, cpu.map(Placement::cpu), None)
    }

    pub(crate) fn start_with<F>(
        name: String,
        cfg: F,
        placement: Option<Placement>,
        metrics: Option<Arc<WorkerSlot>>,
    ) -> Worker<T>
    where
        T: Send + 'static,
        F: ServerConfiguration<Item = T>,
    {
        let (reqs, r_rx) = unbounded();
        let (stop, s_rx) = unbounded();
        let (avail, a_tx) = WorkerAvailability::create();
        let n = name.clone();
        let inner = avail.inner.clone();
        let cpus = Arc::new(OnceLock::new());

        let n2 = name.clone();
        let cpus2 = cpus.clone();
        let init = move || {
            if let Some(placement) = placement {
                if placement.apply() {
                    log::info!("Set affinity to {placement} for worker {n2:?}");
                    // the worker is not available before init completes,
                    // the balancer reads the CPUs once it is
                    let _ = cpus2.set(placement.cpus().into());
                } else {
                    log::warn!("Cannot set affinity to {placement} for worker {n2:?}");
                }
            }
        };

        // placement is applied before the runtime is created, so its memory
        // is allocated on the NUMA node of the worker's CPU
        let arbiter =
            Arbiter::with_init(name.clone(), init).on_stop(move || inner.set_failed(true));
        if let Some(ref m) = metrics {
            m.set_arbiter(arbiter.clone());
        }
        let worker = Worker {
            reqs,
            queue: r_rx.clone(),
            avail,
            stop,
            name,
            arbiter: arbiter.clone(),
            metrics: metrics.clone(),
            cpus,
        };
        let reqs = Queue(r_rx);

        arbiter.handle().spawn_detached(async move {
            log::info!("Starting worker {n:?}");

            // the arbiter is stopped even if the service panics, runtimes
            // that catch task panics keep the arbiter running otherwise
            WorkerSlot::set_current(metrics.clone());
            let _ = spawn(async move {
                match ServiceRunner::create(&n, cfg, reqs, s_rx, a_tx, metrics).await {
                    Ok(wrk) => {
                        log::debug!("Server instance has been created in {n:?}");
                        wrk.run().await;
                    }
                    Err(()) => {
                        log::error!("Cannot start worker {n:?}");
                    }
                }
            })
            .await;
            Arbiter::current().stop();
        });

        worker
    }

    /// Returns the worker name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the arbiter the worker runs on.
    pub(crate) fn arbiter(&self) -> &Arbiter {
        &self.arbiter
    }

    /// Returns the number of open connections plus queued items.
    pub(crate) fn load(&self) -> usize {
        self.metrics.as_ref().map_or(0, |m| m.active()) + self.reqs.len()
    }

    /// Returns the index of the worker's CPU followed by its SMT siblings,
    /// empty if the worker is not placed on a CPU or the placement failed.
    pub(crate) fn cpus(&self) -> &[usize] {
        self.cpus.get().map_or(&[], |cpus| cpus)
    }

    #[cfg(test)]
    pub(crate) fn set_cpus(&self, cpus: &[usize]) {
        let _ = self.cpus.set(cpus.into());
    }

    #[inline]
    /// Sends a message to the worker.
    ///
    /// Returns `Ok` if the message was queued. The message is queued even if
    /// the worker is currently unavailable. Returns the message as `Err` if
    /// the worker has stopped.
    pub fn send(&self, msg: T) -> Result<(), T> {
        self.reqs.try_send(msg).map_err(TrySendError::into_inner)
    }

    /// Takes the items queued for the worker that it has not received yet.
    ///
    /// Takes at most the items queued when called. If `close` is set, the
    /// queue is closed first, so every queued item is taken and later sends
    /// fail.
    pub(crate) fn take_queued(&self, close: bool) -> Vec<T> {
        if close {
            self.queue.close();
        }
        (0..self.queue.len())
            .map_while(|_| self.queue.try_recv().ok())
            .collect()
    }

    /// Returns the current worker status.
    pub fn status(&self) -> WorkerStatus {
        self.avail.inner.status()
    }

    /// Waits for a status change not yet reported by this handle and
    /// returns the current status.
    ///
    /// Each handle tracks the changes it has reported, so every clone
    /// observes every change. A clone starts with the reported changes of
    /// the handle it was cloned from. Several changes between calls are
    /// reported once. Returns immediately if the worker has failed.
    pub async fn wait_for_status(&mut self) -> WorkerStatus {
        self.avail.wait_for_update().await
    }

    /// Stops the worker.
    ///
    /// The worker stops accepting items and shuts down its service, waiting
    /// up to `timeout`. A zero timeout uses the default of 3 seconds.
    pub fn stop(&self, timeout: Millis) -> WorkerStop {
        let (result, rx) = oneshot::async_channel();
        let _ = self.stop.try_send(Shutdown { timeout, result });
        WorkerStop(rx)
    }
}

impl<T> Eq for Worker<T> {}

impl<T> PartialEq for Worker<T> {
    fn eq(&self, other: &Worker<T>) -> bool {
        self.name == other.name
    }
}

impl<T> cmp::Ord for Worker<T> {
    fn cmp(&self, other: &Self) -> cmp::Ordering {
        self.name.cmp(&other.name)
    }
}

impl<T> cmp::PartialOrd for Worker<T> {
    fn partial_cmp(&self, other: &Self) -> Option<cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<T> hash::Hash for Worker<T> {
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        self.name.hash(state);
    }
}

impl<T> Clone for Worker<T> {
    fn clone(&self) -> Self {
        Worker {
            name: self.name.clone(),
            reqs: self.reqs.clone(),
            queue: self.queue.clone(),
            stop: self.stop.clone(),
            avail: self.avail.clone(),
            arbiter: self.arbiter.clone(),
            metrics: self.metrics.clone(),
            cpus: self.cpus.clone(),
        }
    }
}

impl Future for WorkerStop {
    type Output = bool;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match ready!(Pin::new(&mut self.0).poll(cx)) {
            Ok(res) => Poll::Ready(res),
            Err(_) => Poll::Ready(true),
        }
    }
}

#[derive(Debug, Clone)]
struct WorkerAvailability {
    inner: Arc<Inner>,
    /// The last state reported by this handle.
    seen: u64,
}

#[derive(Debug, Clone)]
struct WorkerAvailabilityTx {
    inner: Arc<Inner>,
}

const UNAVAILABLE: u64 = 0;
const AVAILABLE: u64 = 1;
const FAILED: u64 = 2;
const STATUS_BITS: u32 = 2;
const STATUS_MASK: u64 = (1 << STATUS_BITS) - 1;

#[derive(Debug)]
struct Inner {
    /// Change counter in the high bits, status in the low bits.
    state: AtomicU64,
    event: Event,
}

fn status(state: u64) -> WorkerStatus {
    match state & STATUS_MASK {
        AVAILABLE => WorkerStatus::Available,
        FAILED => WorkerStatus::Failed,
        _ => WorkerStatus::Unavailable,
    }
}

impl Inner {
    fn status(&self) -> WorkerStatus {
        status(self.state.load(Ordering::Acquire))
    }

    /// Sets the status, returns `true` if it has changed.
    ///
    /// `Failed` is terminal.
    fn update(&self, st: u64) -> bool {
        let mut cur = self.state.load(Ordering::Acquire);
        loop {
            let old = cur & STATUS_MASK;
            if old == st || old == FAILED {
                return false;
            }
            let next = (((cur >> STATUS_BITS).wrapping_add(1)) << STATUS_BITS) | st;
            match self
                .state
                .compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return true,
                Err(val) => cur = val,
            }
        }
    }

    fn set_failed(&self, notify: bool) {
        self.update(FAILED);
        if notify {
            self.event.notify(usize::MAX);
        }
    }
}

impl WorkerAvailability {
    fn create() -> (Self, WorkerAvailabilityTx) {
        let inner = Arc::new(Inner {
            state: AtomicU64::new(UNAVAILABLE),
            event: Event::new(),
        });

        let avail = WorkerAvailability {
            inner: inner.clone(),
            seen: UNAVAILABLE,
        };
        let avail_tx = WorkerAvailabilityTx { inner };
        (avail, avail_tx)
    }

    fn changed(&mut self) -> Option<WorkerStatus> {
        let cur = self.inner.state.load(Ordering::Acquire);
        let st = status(cur);
        if cur != self.seen || st == WorkerStatus::Failed {
            self.seen = cur;
            Some(st)
        } else {
            None
        }
    }

    async fn wait_for_update(&mut self) -> WorkerStatus {
        loop {
            if let Some(st) = self.changed() {
                return st;
            }
            let listener = self.inner.event.listen();
            if let Some(st) = self.changed() {
                return st;
            }
            listener.await;
        }
    }
}

impl WorkerAvailabilityTx {
    fn set(&self, val: bool) {
        if self.inner.update(if val { AVAILABLE } else { UNAVAILABLE }) {
            self.inner.event.notify(usize::MAX);
        }
    }
}

impl Drop for WorkerAvailabilityTx {
    fn drop(&mut self) {
        // the arbiter `on_stop` callback wakes the waiters. compio aborts the
        // process if a task on another thread is woken during unwinding
        self.inner.set_failed(!std::thread::panicking());
    }
}

/// The worker's end of its item queue.
///
/// [`Worker`] handles keep the queue open, it is closed when the worker drops
/// its end, so sends to a stopped worker fail.
struct Queue<T>(Receiver<T>);

impl<T> Drop for Queue<T> {
    fn drop(&mut self) {
        self.0.close();
    }
}

/// Service runner.
///
/// The runner receives messages through an unbounded channel and processes them.
struct ServiceRunner<F: ServerConfiguration<Item = Req>, Req> {
    name: String,
    factory: F,
    svc: Pipeline<Req, (), ()>,
    reqs: Queue<Req>,
    stop: Pin<Box<dyn Stream<Item = Shutdown>>>,
    availability: WorkerAvailabilityTx,
    metrics: Option<Arc<WorkerSlot>>,
}

impl<F, Req> ServiceRunner<F, Req>
where
    Req: Send + 'static,
    F: ServerConfiguration<Item = Req> + 'static,
{
    async fn create(
        name: &str,
        factory: F,
        reqs: Queue<Req>,
        stop: Receiver<Shutdown>,
        availability: WorkerAvailabilityTx,
        metrics: Option<Arc<WorkerSlot>>,
    ) -> Result<Self, ()> {
        availability.set(false);
        let mut stop = Box::pin(stop);

        let svc = match select(factory.create(), stream_recv(&mut stop)).await {
            Either::Left(Ok(svc)) => Pipeline::new((), svc),
            Either::Right(Some(Shutdown { result, .. })) => {
                log::trace!("Shutdown uninitialized worker");
                let _ = result.send(false);
                return Err(());
            }
            Either::Left(Err(_)) | Either::Right(None) => return Err(()),
        };
        availability.set(true);

        Ok(ServiceRunner {
            factory,
            svc,
            reqs,
            stop,
            availability,
            metrics,
            name: name.to_string(),
        })
    }

    async fn run(mut self) {
        loop {
            let mut recv = std::pin::pin!(self.reqs.0.recv());
            let fut = poll_fn(|cx| {
                match self.svc.poll_ready(cx) {
                    Poll::Ready(Ok(())) => {
                        self.availability.set(true);
                        if let Some(ref m) = self.metrics {
                            m.ready(true);
                        }
                    }
                    Poll::Ready(Err(err)) => {
                        self.availability.set(false);
                        return Poll::Ready(Err(err));
                    }
                    Poll::Pending => {
                        self.availability.set(false);
                        if let Some(ref m) = self.metrics {
                            m.ready(false);
                        }
                        return Poll::Pending;
                    }
                }

                if let Ok(item) = ready!(recv.as_mut().poll(cx)) {
                    Poll::Ready(Ok(Some(item)))
                } else {
                    log::error!("Server is gone");
                    Poll::Ready(Ok(None))
                }
            });

            match select(fut, stream_recv(&mut self.stop)).await {
                Either::Left(Ok(Some(item))) => {
                    // got item
                    if let Some(ref m) = self.metrics {
                        m.item();
                    }
                    let _ = self.svc.call(item).await;
                    continue;
                }
                Either::Left(Err(())) => {
                    // re-create service
                    ntex_rt::spawn_detached(async move {
                        self.svc.shutdown().await;
                    });
                }
                Either::Right(Some(Shutdown { timeout, result })) => {
                    log::info!("Shutting down {:?} worker gracefuly", self.name);
                    self.availability.set(false);

                    let timeout = if timeout.is_zero() { STOP_TIMEOUT } else { timeout };
                    self.stop(timeout, Some(result)).await;
                    return;
                }
                Either::Left(Ok(None)) | Either::Right(None) => {
                    log::info!("Shutting down {:?} worker", self.name);
                    self.availability.set(false);
                    self.stop(STOP_TIMEOUT, None).await;
                    return;
                }
            }

            // re-create service
            loop {
                match select(self.factory.create(), stream_recv(&mut self.stop)).await {
                    Either::Left(Ok(service)) => {
                        self.svc = Pipeline::new((), service);
                        break;
                    }
                    Either::Left(Err(_)) => sleep(Millis::ONE_SEC).await,
                    Either::Right(_) => return,
                }
            }
        }
    }

    async fn stop(&self, timeout: Millis, result: Option<oneshot::Sender<bool>>) {
        // a stopped worker is not counted as not ready
        if let Some(ref m) = self.metrics {
            m.ready(true);
        }
        let res = timeout_checked(timeout, self.svc.shutdown()).await;
        if let Some(result) = result {
            let _ = result.send(res.is_ok());
        }

        log::info!("Worker {:?} has been stopped", self.name);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unused_async_trait_impl)]
    use std::sync::atomic::AtomicUsize;
    use std::task::{Wake, Waker};

    use ntex_service::{Ctx, Service};

    use super::*;

    #[derive(Default)]
    struct WakeCount(AtomicUsize);

    impl Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn poll<F: Future>(fut: Pin<&mut F>, wake: &Arc<WakeCount>) -> Poll<F::Output> {
        let waker = Waker::from(wake.clone());
        fut.poll(&mut Context::from_waker(&waker))
    }

    /// Every clone observes every status change.
    #[test]
    fn status_cloned_observers() {
        let (mut a, tx) = WorkerAvailability::create();
        let mut b = a.clone();
        let (w1, w2) = (
            Arc::new(WakeCount::default()),
            Arc::new(WakeCount::default()),
        );
        {
            let mut first = std::pin::pin!(a.wait_for_update());
            let mut second = std::pin::pin!(b.wait_for_update());
            assert!(poll(first.as_mut(), &w1).is_pending());
            assert!(poll(second.as_mut(), &w2).is_pending());

            tx.set(true);
            assert_eq!(w1.0.load(Ordering::SeqCst), 1);
            assert_eq!(w2.0.load(Ordering::SeqCst), 1);
            assert_eq!(
                poll(second.as_mut(), &w2),
                Poll::Ready(WorkerStatus::Available)
            );
            assert_eq!(
                poll(first.as_mut(), &w1),
                Poll::Ready(WorkerStatus::Available)
            );
        }

        // reported changes are not reported again
        let mut c = a.clone();
        assert!(poll(std::pin::pin!(a.wait_for_update()), &w1).is_pending());
        assert!(poll(std::pin::pin!(c.wait_for_update()), &w1).is_pending());

        // a change between calls is reported, with the current status
        tx.set(false);
        tx.set(true);
        tx.set(false);
        let st = poll(std::pin::pin!(a.wait_for_update()), &w1);
        assert_eq!(st, Poll::Ready(WorkerStatus::Unavailable));
        assert!(poll(std::pin::pin!(a.wait_for_update()), &w1).is_pending());

        // failed is terminal and reported immediately to every clone
        let mut d = a.clone();
        let mut fut = std::pin::pin!(d.wait_for_update());
        assert!(poll(fut.as_mut(), &w2).is_pending());
        drop(tx);
        assert_eq!(poll(fut.as_mut(), &w2), Poll::Ready(WorkerStatus::Failed));
        for wrk in [&mut a, &mut b, &mut c] {
            for _ in 0..2 {
                let st = poll(std::pin::pin!(wrk.wait_for_update()), &w1);
                assert_eq!(st, Poll::Ready(WorkerStatus::Failed));
            }
        }
        assert_eq!(a.inner.status(), WorkerStatus::Failed);
    }

    /// The status cannot change after the worker has failed.
    #[test]
    fn status_failed_is_terminal() {
        let (a, tx) = WorkerAvailability::create();
        a.inner.set_failed(false);
        tx.set(true);
        assert_eq!(a.inner.status(), WorkerStatus::Failed);
    }

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

    /// The CPUs of a worker are known only if its placement is applied.
    #[ntex::test]
    async fn worker_cpus() {
        let cpu = crate::affinity::plan(crate::Affinity::Cpu).pop();
        for (placement, placed) in [
            (Placement::cpu(usize::MAX >> 1), false),
            (
                cpu.unwrap_or_else(|| Placement::cpu(0)),
                cfg!(any(windows, target_os = "linux", target_os = "android")),
            ),
        ] {
            let mut wrk = Worker::start_with("test".into(), Cfg, Some(placement), None);
            assert_eq!(wrk.wait_for_status().await, WorkerStatus::Available);
            assert_eq!(!wrk.cpus().is_empty(), placed);
            wrk.stop(Millis::ZERO).await;
        }
    }
}
