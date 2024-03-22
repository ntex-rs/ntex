use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{ready, Context, Poll};
use std::{cmp, future::poll_fn, future::Future, hash, pin::Pin, sync::Arc};

use async_broadcast::{self as bus, broadcast};
use async_channel::{unbounded, Receiver, Sender};

use ntex_rt::{spawn, Arbiter};
use ntex_service::{Pipeline, ServiceFactory};
use ntex_util::future::{select, stream_recv, Either, Stream};
use ntex_util::time::{sleep, timeout_checked, Millis};

use crate::{ServerConfiguration, WorkerId, WorkerMessage};

const STOP_TIMEOUT: Millis = Millis::ONE_SEC;

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
#[must_use = "Future do nothing unless polled"]
/// Stop worker process
///
/// Stop future resolves when worker completes processing
/// incoming items and stop arbiter
pub struct WorkerStop(oneshot::Receiver<bool>);

impl<T> Worker<T> {
    /// Start worker.
    pub fn start<F>(id: WorkerId, cfg: F) -> Worker<T>
    where
        T: Send + 'static,
        F: ServerConfiguration<Item = T>,
    {
        let (tx1, rx1) = unbounded();
        let (tx2, rx2) = unbounded();
        let (avail, avail_tx) = WorkerAvailability::create();

        Arbiter::default().exec_fn(move || {
            let _ = spawn(async move {
                let factory = cfg.create().await;

                match create(id, rx1, rx2, factory, avail_tx).await {
                    Ok((svc, wrk)) => {
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
            // cleanup updates
            while self.avail.notify.try_recv().is_ok() {}

            if self.avail.notify.recv_direct().await.is_err() {
                self.failed.store(true, Ordering::Release);
            }
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
    notify: bus::Receiver<()>,
    available: Arc<AtomicBool>,
}

#[derive(Debug, Clone)]
struct WorkerAvailabilityTx {
    notify: bus::Sender<()>,
    available: Arc<AtomicBool>,
}

impl WorkerAvailability {
    fn create() -> (Self, WorkerAvailabilityTx) {
        let (mut tx, rx) = broadcast(16);
        tx.set_overflow(true);

        let avail = WorkerAvailability {
            notify: rx,
            available: Arc::new(AtomicBool::new(false)),
        };
        let avail_tx = WorkerAvailabilityTx {
            notify: tx,
            available: avail.available.clone(),
        };
        (avail, avail_tx)
    }

    fn available(&self) -> bool {
        self.available.load(Ordering::Acquire)
    }
}

impl WorkerAvailabilityTx {
    fn set(&self, val: bool) {
        let old = self.available.swap(val, Ordering::Release);
        if !old && val {
            let _ = self.notify.try_broadcast(());
        }
    }
}

/// Service worker
///
/// Worker accepts message via unbounded channel and starts processing.
struct WorkerSt<T, F: ServiceFactory<WorkerMessage<T>>> {
    id: WorkerId,
    rx: Pin<Box<dyn Stream<Item = T>>>,
    stop: Pin<Box<dyn Stream<Item = Shutdown>>>,
    factory: F,
    availability: WorkerAvailabilityTx,
}

async fn run_worker<T, F>(mut svc: Pipeline<F::Service>, mut wrk: WorkerSt<T, F>)
where
    T: Send + 'static,
    F: ServiceFactory<WorkerMessage<T>> + 'static,
{
    loop {
        let fut = poll_fn(|cx| {
            ready!(svc.poll_ready(cx)?);

            if let Some(item) = ready!(Pin::new(&mut wrk.rx).poll_next(cx)) {
                let fut = svc.call_static(WorkerMessage::New(item));
                let _ = spawn(async move {
                    let _ = fut.await;
                });
            }
            Poll::Ready(Ok::<(), F::Error>(()))
        });

        match select(fut, stream_recv(&mut wrk.stop)).await {
            Either::Left(Ok(())) => continue,
            Either::Left(Err(_)) => {
                wrk.availability.set(false);
            }
            Either::Right(Some(Shutdown { timeout, result })) => {
                wrk.availability.set(false);

                if timeout.is_zero() {
                    let fut = svc.call_static(WorkerMessage::ForceShutdown);
                    let _ = spawn(async move {
                        let _ = fut.await;
                    });
                    sleep(STOP_TIMEOUT).await;
                } else {
                    let fut = svc.call_static(WorkerMessage::Shutdown(timeout));
                    let res = timeout_checked(timeout, fut).await;
                    let _ = result.send(res.is_ok());
                };
                poll_fn(|cx| svc.poll_shutdown(cx)).await;

                log::info!("Stopping worker {:?}", wrk.id);
                return;
            }
            Either::Right(None) => return,
        }

        loop {
            match select(wrk.factory.create(()), stream_recv(&mut wrk.stop)).await {
                Either::Left(Ok(service)) => {
                    wrk.availability.set(true);
                    svc = Pipeline::new(service);
                    break;
                }
                Either::Left(Err(_)) => sleep(STOP_TIMEOUT).await,
                Either::Right(_) => return,
            }
        }
    }
}

async fn create<T, F>(
    id: WorkerId,
    rx: Receiver<T>,
    stop: Receiver<Shutdown>,
    factory: Result<F, ()>,
    availability: WorkerAvailabilityTx,
) -> Result<(Pipeline<F::Service>, WorkerSt<T, F>), ()>
where
    T: Send + 'static,
    F: ServiceFactory<WorkerMessage<T>> + 'static,
{
    log::info!("Starting worker {:?}", id);

    availability.set(false);
    let factory = factory?;

    let rx = Box::pin(rx);
    let mut stop = Box::pin(stop);

    let svc = match select(factory.create(()), stream_recv(&mut stop)).await {
        Either::Left(Ok(svc)) => Pipeline::new(svc),
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
            factory,
            availability,
            rx: Box::pin(rx),
            stop: Box::pin(stop),
        },
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::io::Io;
    use crate::server::accept::AcceptNotify;
    use crate::server::service::Factory;
    use crate::service::{Service, ServiceCtx, ServiceFactory};
    use crate::util::lazy;

    #[derive(Clone, Copy, Debug)]
    enum St {
        Fail,
        Ready,
        Pending,
    }

    #[derive(Clone)]
    struct SrvFactory {
        st: Arc<Mutex<St>>,
        counter: Arc<Mutex<usize>>,
    }

    impl ServiceFactory<Io> for SrvFactory {
        type Response = ();
        type Error = ();
        type Service = Srv;
        type InitError = ();

        async fn create(&self, _: ()) -> Result<Srv, ()> {
            let mut cnt = self.counter.lock().unwrap();
            *cnt += 1;
            Ok(Srv {
                st: self.st.clone(),
            })
        }
    }

    struct Srv {
        st: Arc<Mutex<St>>,
    }

    impl Service<Io> for Srv {
        type Response = ();
        type Error = ();

        fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            let st: St = { *self.st.lock().unwrap() };
            match st {
                St::Fail => {
                    *self.st.lock().unwrap() = St::Pending;
                    Poll::Ready(Err(()))
                }
                St::Ready => Poll::Ready(Ok(())),
                St::Pending => Poll::Pending,
            }
        }

        fn poll_shutdown(&self, _: &mut Context<'_>) -> Poll<()> {
            match *self.st.lock().unwrap() {
                St::Ready => Poll::Ready(()),
                St::Fail | St::Pending => Poll::Pending,
            }
        }

        async fn call(&self, _: Io, _: ServiceCtx<'_, Self>) -> Result<(), ()> {
            Ok(())
        }
    }

    #[crate::rt_test]
    #[allow(clippy::mutex_atomic)]
    async fn basics() {
        let (_tx1, rx1) = unbounded();
        let (tx2, rx2) = unbounded();
        let (sync_tx, _sync_rx) = std::sync::mpsc::channel();
        let poll = Arc::new(polling::Poller::new().unwrap());
        let waker = poll.clone();
        let avail = WorkerAvailability::new(Box::new(AcceptNotify::new(
            waker.clone(),
            sync_tx.clone(),
        )));

        let st = Arc::new(Mutex::new(St::Pending));
        let counter = Arc::new(Mutex::new(0));

        let f = SrvFactory {
            st: st.clone(),
            counter: counter.clone(),
        };

        let mut worker = Worker::create(
            rx1,
            rx2,
            vec![Factory::create(
                "test".to_string(),
                Token(0),
                move |_| f.clone(),
                "127.0.0.1:8080".parse().unwrap(),
                "TEST",
            )],
            avail.clone(),
            Millis(5_000),
        )
        .await
        .unwrap();
        assert_eq!(*counter.lock().unwrap(), 1);

        let _ = lazy(|cx| Pin::new(&mut worker).poll(cx)).await;
        assert!(!avail.available());

        let _ = lazy(|cx| Pin::new(&mut worker).poll(cx)).await;
        assert!(!avail.available());

        *st.lock().unwrap() = St::Ready;
        let _ = lazy(|cx| Pin::new(&mut worker).poll(cx)).await;
        assert!(avail.available());

        *st.lock().unwrap() = St::Pending;
        let _ = lazy(|cx| Pin::new(&mut worker).poll(cx)).await;
        assert!(!avail.available());

        *st.lock().unwrap() = St::Ready;
        let _ = lazy(|cx| Pin::new(&mut worker).poll(cx)).await;
        assert!(avail.available());

        // restart
        *st.lock().unwrap() = St::Fail;
        let _ = lazy(|cx| Pin::new(&mut worker).poll(cx)).await;
        assert!(!avail.available());

        *st.lock().unwrap() = St::Fail;
        let _ = lazy(|cx| Pin::new(&mut worker).poll(cx)).await;
        assert!(!avail.available());

        *st.lock().unwrap() = St::Ready;
        let _ = lazy(|cx| Pin::new(&mut worker).poll(cx)).await;
        assert!(avail.available());

        // shutdown
        let g = MAX_CONNS_COUNTER.with(|conns| conns.get());

        let (tx, rx) = oneshot::channel();
        tx2.try_send(StopCommand {
            graceful: true,
            result: tx,
        })
        .unwrap();

        let _ = lazy(|cx| Pin::new(&mut worker).poll(cx)).await;
        assert!(!avail.available());
        drop(g);
        assert!(lazy(|cx| Pin::new(&mut worker).poll(cx)).await.is_ready());
        let _ = rx.await;

        // force shutdown
        let (_tx1, rx1) = unbounded();
        let (tx2, rx2) = unbounded();
        let avail =
            WorkerAvailability::new(Box::new(AcceptNotify::new(waker, sync_tx.clone())));
        let f = SrvFactory {
            st: st.clone(),
            counter: counter.clone(),
        };

        let mut worker = Worker::create(
            rx1,
            rx2,
            vec![Factory::create(
                "test".to_string(),
                Token(0),
                move |_| f.clone(),
                "127.0.0.1:8080".parse().unwrap(),
                "TEST",
            )],
            avail.clone(),
            Millis(5_000),
        )
        .await
        .unwrap();

        // shutdown
        let _g = MAX_CONNS_COUNTER.with(|conns| conns.get());

        *st.lock().unwrap() = St::Ready;
        let _ = lazy(|cx| Pin::new(&mut worker).poll(cx)).await;
        assert!(avail.available());

        let (tx, rx) = oneshot::channel();
        tx2.try_send(StopCommand {
            graceful: false,
            result: tx,
        })
        .unwrap();

        assert!(lazy(|cx| Pin::new(&mut worker).poll(cx)).await.is_ready());
        let _ = rx.await;
    }
}
