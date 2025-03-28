use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{ready, Context, Poll};
use std::{cmp, future::poll_fn, future::Future, hash, pin::Pin, sync::Arc};

use async_channel::{unbounded, Receiver, Sender};
use atomic_waker::AtomicWaker;
use core_affinity::CoreId;

use ntex_rt::{spawn, Arbiter};
use ntex_service::{Pipeline, PipelineBinding, Service, ServiceFactory};
use ntex_util::future::{select, stream_recv, Either, Stream};
use ntex_util::time::{sleep, timeout_checked, Millis};

use crate::{ServerConfiguration, WorkerId};

const STOP_TIMEOUT: Millis = Millis(3000);

#[derive(Debug)]
/// Shutdown worker
struct Shutdown {
    timeout: Millis,
    result: oneshot::Sender<bool>,
}

#[derive(Copy, Clone, Default, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
/// Worker status
pub enum WorkerStatus {
    Available,
    #[default]
    Unavailable,
    Failed,
}

#[derive(Debug)]
/// Server worker
///
/// Worker accepts message via unbounded channel and starts processing.
pub struct Worker<T> {
    id: WorkerId,
    tx1: Sender<T>,
    tx2: Sender<Shutdown>,
    avail: WorkerAvailability,
    failed: Arc<AtomicBool>,
}

impl<T> cmp::Ord for Worker<T> {
    fn cmp(&self, other: &Self) -> cmp::Ordering {
        self.id.cmp(&other.id)
    }
}

impl<T> cmp::PartialOrd for Worker<T> {
    fn partial_cmp(&self, other: &Self) -> Option<cmp::Ordering> {
        Some(self.id.cmp(&other.id))
    }
}

impl<T> hash::Hash for Worker<T> {
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

impl<T> Eq for Worker<T> {}

impl<T> PartialEq for Worker<T> {
    fn eq(&self, other: &Worker<T>) -> bool {
        self.id == other.id
    }
}

#[derive(Debug)]
/// Stop worker process
///
/// Stop future resolves when worker completes processing
/// incoming items and stop arbiter
pub struct WorkerStop(oneshot::Receiver<bool>);

impl<T> Worker<T> {
    /// Start worker.
    pub fn start<F>(id: WorkerId, cfg: F, cid: Option<CoreId>) -> Worker<T>
    where
        T: Send + 'static,
        F: ServerConfiguration<Item = T>,
    {
        let (tx1, rx1) = unbounded();
        let (tx2, rx2) = unbounded();
        let (avail, avail_tx) = WorkerAvailability::create();

        Arbiter::default().exec_fn(move || {
            if let Some(cid) = cid {
                if core_affinity::set_for_current(cid) {
                    log::info!("Set affinity to {:?} for worker {:?}", cid, id);
                }
            }

            let _ = spawn(async move {
                log::info!("Starting worker {:?}", id);

                log::debug!("Creating server instance in {:?}", id);
                let factory = cfg.create().await;

                match create(id, rx1, rx2, factory, avail_tx).await {
                    Ok((svc, wrk)) => {
                        log::debug!("Server instance has been created in {:?}", id);
                        run_worker(svc, wrk).await;
                    }
                    Err(e) => {
                        log::error!("Cannot start worker: {:?}", e);
                    }
                }
                Arbiter::current().stop();
            });
        });

        Worker {
            id,
            tx1,
            tx2,
            avail,
            failed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Worker id.
    pub fn id(&self) -> WorkerId {
        self.id
    }

    /// Send message to the worker.
    ///
    /// Returns `Ok` if message got accepted by the worker.
    /// Otherwise return message back as `Err`
    pub fn send(&self, msg: T) -> Result<(), T> {
        self.tx1.try_send(msg).map_err(|msg| msg.into_inner())
    }

    /// Check worker status.
    pub fn status(&self) -> WorkerStatus {
        if self.failed.load(Ordering::Acquire) {
            WorkerStatus::Failed
        } else if self.avail.available() {
            WorkerStatus::Available
        } else {
            WorkerStatus::Unavailable
        }
    }

    /// Wait for worker status updates
    pub async fn wait_for_status(&mut self) -> WorkerStatus {
        if self.failed.load(Ordering::Acquire) {
            WorkerStatus::Failed
        } else {
            self.avail.wait_for_update().await;
            if self.avail.failed() {
                self.failed.store(true, Ordering::Release);
            }
            println!("-------- update status {:?}", self.status());
            self.status()
        }
    }

    /// Stop worker.
    ///
    /// If timeout value is zero, force shutdown worker
    pub fn stop(&self, timeout: Millis) -> WorkerStop {
        let (result, rx) = oneshot::channel();
        let _ = self.tx2.try_send(Shutdown { timeout, result });
        WorkerStop(rx)
    }
}

impl<T> Clone for Worker<T> {
    fn clone(&self) -> Self {
        Worker {
            id: self.id,
            tx1: self.tx1.clone(),
            tx2: self.tx2.clone(),
            avail: self.avail.clone(),
            failed: self.failed.clone(),
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
            if self.inner.updated.load(Ordering::Acquire) {
                println!("-------- status updated");
                self.inner.updated.store(false, Ordering::Release);
                Poll::Ready(())
            } else {
                self.inner.waker.register(cx.waker());
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

/// Service worker
///
/// Worker accepts message via unbounded channel and starts processing.
struct WorkerSt<T, F: ServiceFactory<T>> {
    id: WorkerId,
    rx: Receiver<T>,
    stop: Pin<Box<dyn Stream<Item = Shutdown>>>,
    factory: F,
    availability: WorkerAvailabilityTx,
}

async fn run_worker<T, F>(mut svc: PipelineBinding<F::Service, T>, mut wrk: WorkerSt<T, F>)
where
    T: Send + 'static,
    F: ServiceFactory<T> + 'static,
{
    loop {
        let mut recv = std::pin::pin!(wrk.rx.recv());
        let fut = poll_fn(|cx| {
            match svc.poll_ready(cx) {
                Poll::Ready(Ok(())) => {
                    println!("-------- status updated: ready");
                    wrk.availability.set(true);
                }
                Poll::Ready(Err(err)) => {
                    println!("-------- status updated: failed");
                    wrk.availability.set(false);
                    return Poll::Ready(Err(err));
                }
                Poll::Pending => {
                    println!("-------- status updated: pending");
                    wrk.availability.set(false);
                    return Poll::Pending;
                }
            }

            match ready!(recv.as_mut().poll(cx)) {
                Ok(item) => {
                    let fut = svc.call(item);
                    let _ = spawn(async move {
                        let _ = fut.await;
                    });
                    Poll::Ready(Ok::<_, F::Error>(true))
                }
                Err(_) => {
                    log::error!("Server is gone");
                    Poll::Ready(Ok(false))
                }
            }
        });

        match select(fut, stream_recv(&mut wrk.stop)).await {
            Either::Left(Ok(true)) => continue,
            Either::Left(Err(_)) => {
                let _ = ntex_rt::spawn(async move {
                    svc.shutdown().await;
                });
            }
            Either::Right(Some(Shutdown { timeout, result })) => {
                wrk.availability.set(false);

                let timeout = if timeout.is_zero() {
                    STOP_TIMEOUT
                } else {
                    timeout
                };

                stop_svc(wrk.id, svc, timeout, Some(result)).await;
                return;
            }
            Either::Left(Ok(false)) | Either::Right(None) => {
                wrk.availability.set(false);
                stop_svc(wrk.id, svc, STOP_TIMEOUT, None).await;
                return;
            }
        }

        // re-create service
        loop {
            match select(wrk.factory.create(()), stream_recv(&mut wrk.stop)).await {
                Either::Left(Ok(service)) => {
                    svc = Pipeline::new(service).bind();
                    break;
                }
                Either::Left(Err(_)) => sleep(Millis::ONE_SEC).await,
                Either::Right(_) => return,
            }
        }
    }
}

async fn stop_svc<T, F>(
    id: WorkerId,
    svc: PipelineBinding<F, T>,
    timeout: Millis,
    result: Option<oneshot::Sender<bool>>,
) where
    T: Send + 'static,
    F: Service<T> + 'static,
{
    let res = timeout_checked(timeout, svc.shutdown()).await;
    if let Some(result) = result {
        let _ = result.send(res.is_ok());
    }

    log::info!("Worker {:?} has been stopped", id);
}

async fn create<T, F>(
    id: WorkerId,
    rx: Receiver<T>,
    stop: Receiver<Shutdown>,
    factory: Result<F, ()>,
    availability: WorkerAvailabilityTx,
) -> Result<(PipelineBinding<F::Service, T>, WorkerSt<T, F>), ()>
where
    T: Send + 'static,
    F: ServiceFactory<T> + 'static,
{
    availability.set(false);
    let factory = factory?;
    let mut stop = Box::pin(stop);

    let svc = match select(factory.create(()), stream_recv(&mut stop)).await {
        Either::Left(Ok(svc)) => Pipeline::new(svc).bind(),
        Either::Left(Err(_)) => return Err(()),
        Either::Right(Some(Shutdown { result, .. })) => {
            log::trace!("Shutdown uninitialized worker");
            let _ = result.send(false);
            return Err(());
        }
        Either::Right(None) => return Err(()),
    };
    availability.set(true);

    Ok((
        svc,
        WorkerSt {
            id,
            rx,
            factory,
            availability,
            stop: Box::pin(stop),
        },
    ))
}
