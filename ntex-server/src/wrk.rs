use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, ready};
use std::{cmp, future::Future, future::poll_fn, hash, pin::Pin, sync::Arc};

use async_channel::{Receiver, Sender, TrySendError, unbounded};
use atomic_waker::AtomicWaker;
use core_affinity::CoreId;

use ntex_rt::{Arbiter, spawn};
use ntex_service::Pipeline;
use ntex_util::future::{Either, Stream, select, stream_recv};
use ntex_util::time::{Millis, sleep, timeout_checked};

use crate::ServerConfiguration;

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
    Available,
    #[default]
    Unavailable,
    Failed,
}

#[derive(Debug)]
/// Server worker.
///
/// Worker accepts message via unbounded channel and starts processing.
pub struct Worker<T> {
    name: String,
    reqs: Sender<T>,
    stop: Sender<Shutdown>,
    avail: WorkerAvailability,
}

#[derive(Debug)]
/// Stop worker process.
///
/// Stop future resolves when worker completes processing
/// incoming items and stop arbiter
pub struct WorkerStop(oneshot::AsyncReceiver<bool>);

impl<T> Worker<T> {
    /// Start worker.
    pub fn start<F>(name: String, cfg: F, cid: Option<CoreId>) -> Worker<T>
    where
        T: Send + 'static,
        F: ServerConfiguration<Item = T>,
    {
        let (reqs, r_rx) = unbounded();
        let (stop, s_rx) = unbounded();
        let (avail, a_tx) = WorkerAvailability::create();
        let n = name.clone();
        let inner = avail.inner.clone();

        let worker = Worker {
            reqs,
            avail,
            stop,
            name: name.clone(),
        };

        Arbiter::with_name(name)
            .on_stop(move || {
                inner.failed.store(true, Ordering::Release);
                inner.updated.store(true, Ordering::Release);
                inner.available.store(false, Ordering::Release);
                inner.waker.wake();
            })
            .handle()
            .spawn(async move {
                log::info!("Starting worker {n:?}");
                if let Some(cid) = cid
                    && core_affinity::set_for_current(cid)
                {
                    log::info!("Set affinity to {cid:?} for worker {n:?}");
                }

                spawn(async move {
                    match ServiceRunner::create(&n, cfg, r_rx, s_rx, a_tx).await {
                        Ok(wrk) => {
                            log::debug!("Server instance has been created in {n:?}");
                            wrk.run().await;
                        }
                        Err(()) => {
                            log::error!("Cannot start worker {n:?}");
                        }
                    }
                    Arbiter::current().stop();
                });
            });

        worker
    }

    /// Worker name
    pub fn name(&self) -> &str {
        &self.name
    }

    #[inline]
    /// Sends a message to the worker.
    ///
    /// Returns `Ok` if the worker accepts the message.
    /// Otherwise, returns the message as `Err`.
    pub fn send(&self, msg: T) -> Result<(), T> {
        self.reqs.try_send(msg).map_err(TrySendError::into_inner)
    }

    /// Check worker status.
    pub fn status(&self) -> WorkerStatus {
        if self.avail.failed() {
            WorkerStatus::Failed
        } else if self.avail.available() {
            WorkerStatus::Available
        } else {
            WorkerStatus::Unavailable
        }
    }

    /// Wait for worker status updates.
    pub async fn wait_for_status(&mut self) -> WorkerStatus {
        if self.avail.failed() {
            WorkerStatus::Failed
        } else {
            self.avail.wait_for_update().await;
            self.status()
        }
    }

    /// Stop the worker.
    ///
    /// If the timeout is zero, forcefully shut down the worker.
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
            stop: self.stop.clone(),
            avail: self.avail.clone(),
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
}

#[derive(Debug, Clone)]
struct WorkerAvailabilityTx {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    waker: AtomicWaker,
    updated: AtomicBool,
    available: AtomicBool,
    failed: AtomicBool,
}

impl WorkerAvailability {
    fn create() -> (Self, WorkerAvailabilityTx) {
        let inner = Arc::new(Inner {
            waker: AtomicWaker::new(),
            updated: AtomicBool::new(false),
            available: AtomicBool::new(false),
            failed: AtomicBool::new(false),
        });

        let avail = WorkerAvailability {
            inner: inner.clone(),
        };
        let avail_tx = WorkerAvailabilityTx { inner };
        (avail, avail_tx)
    }

    fn failed(&self) -> bool {
        self.inner.failed.load(Ordering::Acquire)
    }

    fn available(&self) -> bool {
        self.inner.available.load(Ordering::Acquire)
    }

    async fn wait_for_update(&self) {
        poll_fn(|cx| {
            self.inner.waker.register(cx.waker());
            if self.inner.updated.swap(false, Ordering::AcqRel) {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
    }
}

impl WorkerAvailabilityTx {
    fn set(&self, val: bool) {
        let old = self.inner.available.swap(val, Ordering::Release);
        if old != val {
            self.inner.updated.store(true, Ordering::Release);
            self.inner.waker.wake();
        }
    }
}

impl Drop for WorkerAvailabilityTx {
    fn drop(&mut self) {
        self.inner.failed.store(true, Ordering::Release);
        self.inner.updated.store(true, Ordering::Release);
        self.inner.available.store(false, Ordering::Release);
        self.inner.waker.wake();
    }
}

/// Service runner.
///
/// The runner receives messages through an unbounded channel and processes them.
struct ServiceRunner<F: ServerConfiguration<Item = Req>, Req> {
    name: String,
    factory: F,
    svc: Pipeline<Req, (), ()>,
    reqs: Receiver<Req>,
    stop: Pin<Box<dyn Stream<Item = Shutdown>>>,
    availability: WorkerAvailabilityTx,
}

impl<F, Req> ServiceRunner<F, Req>
where
    Req: Send + 'static,
    F: ServerConfiguration<Item = Req> + 'static,
{
    async fn create(
        name: &str,
        factory: F,
        reqs: Receiver<Req>,
        stop: Receiver<Shutdown>,
        availability: WorkerAvailabilityTx,
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
            name: name.to_string(),
        })
    }

    async fn run(mut self) {
        loop {
            let mut recv = std::pin::pin!(self.reqs.recv());
            let fut = poll_fn(|cx| {
                match self.svc.poll_ready(cx) {
                    Poll::Ready(Ok(())) => {
                        self.availability.set(true);
                    }
                    Poll::Ready(Err(err)) => {
                        self.availability.set(false);
                        return Poll::Ready(Err(err));
                    }
                    Poll::Pending => {
                        self.availability.set(false);
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
                    let _ = self.svc.call(item).await;
                    continue;
                }
                Either::Left(Err(())) => {
                    // re-create service
                    ntex_rt::spawn(async move {
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
        let res = timeout_checked(timeout, self.svc.shutdown()).await;
        if let Some(result) = result {
            let _ = result.send(res.is_ok());
        }

        log::info!("Worker {:?} has been stopped", self.name);
    }
}
