use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::{future::Future, pin::Pin, sync::Arc, task::Context, task::Poll};

use async_channel::{unbounded, Receiver, Sender};
use async_oneshot as oneshot;

use crate::rt::{spawn, Arbiter};
use crate::service::Pipeline;
use crate::time::{sleep, Millis, Sleep};
use crate::util::{
    join_all, ready, select, stream_recv, BoxFuture, Either, Stream as FutStream,
};

use super::accept::{AcceptNotify, Command};
use super::service::{BoxedServerService, InternalServiceFactory, ServerMessage};
use super::{counter::Counter, socket::Stream, Token};

#[derive(Debug)]
pub(super) struct WorkerCommand(Connection);

#[derive(Debug)]
/// Stop worker message. Returns `true` on successful shutdown
/// and `false` if some connections are still alive.
pub(super) struct StopCommand {
    graceful: bool,
    result: oneshot::Sender<bool>,
}

#[derive(Debug)]
pub(super) struct Connection {
    pub(super) io: Stream,
    pub(super) token: Token,
}

const STOP_TIMEOUT: Millis = Millis::ONE_SEC;
static MAX_CONNS: AtomicUsize = AtomicUsize::new(25600);

/// Sets the maximum per-worker number of concurrent connections.
///
/// All socket listeners will stop accepting connections when this limit is
/// reached for each worker.
///
/// By default max connections is set to a 25k per worker.
pub(super) fn max_concurrent_connections(num: usize) {
    MAX_CONNS.store(num, Ordering::Relaxed);
}

pub(super) fn num_connections() -> usize {
    MAX_CONNS_COUNTER.with(|conns| conns.total())
}

thread_local! {
    static MAX_CONNS_COUNTER: Counter =
        Counter::new(MAX_CONNS.load(Ordering::Relaxed));
}

#[derive(Clone, Debug)]
pub(super) struct WorkerClient {
    pub(super) idx: usize,
    tx1: Sender<WorkerCommand>,
    tx2: Sender<StopCommand>,
    avail: WorkerAvailability,
}

impl WorkerClient {
    pub(super) fn new(
        idx: usize,
        tx1: Sender<WorkerCommand>,
        tx2: Sender<StopCommand>,
        avail: WorkerAvailability,
    ) -> Self {
        WorkerClient {
            idx,
            tx1,
            tx2,
            avail,
        }
    }

    pub(super) fn send(&self, msg: Connection) -> Result<(), Connection> {
        self.tx1
            .try_send(WorkerCommand(msg))
            .map_err(|msg| msg.into_inner().0)
    }

    pub(super) fn available(&self) -> bool {
        self.avail.available()
    }

    pub(super) fn stop(&self, graceful: bool) -> oneshot::Receiver<bool> {
        let (result, rx) = oneshot::oneshot();
        let _ = self.tx2.try_send(StopCommand { graceful, result });
        rx
    }
}

#[derive(Debug, Clone)]
pub(super) struct WorkerAvailability {
    notify: AcceptNotify,
    available: Arc<AtomicBool>,
}

impl WorkerAvailability {
    pub(super) fn new(notify: AcceptNotify) -> Self {
        WorkerAvailability {
            notify,
            available: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(super) fn available(&self) -> bool {
        self.available.load(Ordering::Acquire)
    }

    pub(super) fn set(&self, val: bool) {
        let old = self.available.swap(val, Ordering::Release);
        if !old && val {
            self.notify.send(Command::WorkerAvailable)
        }
    }
}

/// Service worker
///
/// Worker accepts Socket objects via unbounded channel and starts stream
/// processing.
pub(super) struct Worker {
    rx: Receiver<WorkerCommand>,
    rx2: Receiver<StopCommand>,
    services: Vec<WorkerService>,
    availability: WorkerAvailability,
    conns: Counter,
    factories: Vec<Box<dyn InternalServiceFactory>>,
    state: WorkerState,
    shutdown_timeout: Millis,
}

struct WorkerService {
    factory: usize,
    status: WorkerServiceStatus,
    service: Pipeline<BoxedServerService>,
}

impl WorkerService {
    fn created(&mut self, service: BoxedServerService) {
        self.service = Pipeline::new(service);
        self.status = WorkerServiceStatus::Unavailable;
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum WorkerServiceStatus {
    Available,
    Unavailable,
    Failed,
    Restarting,
    Stopping,
    Stopped,
}

impl Worker {
    pub(super) fn start(
        idx: usize,
        factories: Vec<Box<dyn InternalServiceFactory>>,
        availability: WorkerAvailability,
        shutdown_timeout: Millis,
    ) -> WorkerClient {
        let (tx1, rx1) = unbounded();
        let (tx2, rx2) = unbounded();
        let avail = availability.clone();

        Arbiter::default().exec_fn(move || {
            spawn(async move {
                match Worker::create(rx1, rx2, factories, availability, shutdown_timeout)
                    .await
                {
                    Ok(wrk) => {
                        spawn(wrk);
                    }
                    Err(e) => {
                        error!("Cannot start worker: {:?}", e);
                        Arbiter::current().stop();
                    }
                }
            });
        });

        WorkerClient::new(idx, tx1, tx2, avail)
    }

    async fn create(
        rx: Receiver<WorkerCommand>,
        rx2: Receiver<StopCommand>,
        factories: Vec<Box<dyn InternalServiceFactory>>,
        availability: WorkerAvailability,
        shutdown_timeout: Millis,
    ) -> Result<Worker, ()> {
        availability.set(false);
        let mut wrk = MAX_CONNS_COUNTER.with(move |conns| Worker {
            rx,
            rx2,
            availability,
            factories,
            shutdown_timeout,
            services: Vec::new(),
            conns: conns.priv_clone(),
            state: WorkerState::Unavailable,
        });

        let mut fut: Vec<BoxFuture<'static, _>> = Vec::new();
        for (idx, factory) in wrk.factories.iter().enumerate() {
            let f = factory.create();
            fut.push(Box::pin(async move {
                let r = f.await?;

                Ok::<_, ()>(
                    r.into_iter()
                        .map(|(t, s): (Token, _)| (idx, t, s))
                        .collect::<Vec<_>>(),
                )
            }));
        }

        let res: Result<Vec<_>, _> =
            match select(join_all(fut), stream_recv(&mut wrk.rx2)).await {
                Either::Left(result) => result.into_iter().collect(),
                Either::Right(Some(StopCommand { mut result, .. })) => {
                    trace!("Shutdown uninitialized worker");
                    wrk.shutdown(true);
                    let _ = result.send(false);
                    return Err(());
                }
                Either::Right(None) => Err(()),
            };
        match res {
            Ok(services) => {
                for item in services {
                    for (factory, token, service) in item {
                        assert_eq!(token.0, wrk.services.len());
                        wrk.services.push(WorkerService {
                            factory,
                            service: service.into(),
                            status: WorkerServiceStatus::Unavailable,
                        });
                    }
                }
                Ok(wrk)
            }
            Err(_) => Err(()),
        }
    }

    fn shutdown(&mut self, force: bool) {
        if force {
            self.services.iter_mut().for_each(|srv| {
                if srv.status == WorkerServiceStatus::Available {
                    srv.status = WorkerServiceStatus::Stopped;
                    let svc = srv.service.clone();
                    spawn(async move {
                        let _ = svc.call((None, ServerMessage::ForceShutdown)).await;
                    });
                }
            });
        } else {
            let timeout = self.shutdown_timeout;
            self.services.iter_mut().for_each(move |srv| {
                if srv.status == WorkerServiceStatus::Available {
                    srv.status = WorkerServiceStatus::Stopping;

                    let svc = srv.service.clone();
                    spawn(async move {
                        let _ = svc.call((None, ServerMessage::Shutdown(timeout))).await;
                    });
                }
            });
        }
    }

    fn check_readiness(&mut self, cx: &mut Context<'_>) -> Result<bool, (Token, usize)> {
        let mut ready = self.conns.available(cx);
        let mut failed = None;
        for (idx, srv) in &mut self.services.iter_mut().enumerate() {
            if srv.status == WorkerServiceStatus::Available
                || srv.status == WorkerServiceStatus::Unavailable
            {
                match srv.service.poll_ready(cx) {
                    Poll::Ready(Ok(_)) => {
                        if srv.status == WorkerServiceStatus::Unavailable {
                            trace!(
                                "Service {:?} is available",
                                self.factories[srv.factory].name(Token(idx))
                            );
                            srv.status = WorkerServiceStatus::Available;
                        }
                    }
                    Poll::Pending => {
                        ready = false;

                        if srv.status == WorkerServiceStatus::Available {
                            trace!(
                                "Service {:?} is unavailable",
                                self.factories[srv.factory].name(Token(idx))
                            );
                            srv.status = WorkerServiceStatus::Unavailable;
                        }
                    }
                    Poll::Ready(Err(_)) => {
                        error!(
                            "Service {:?} readiness check returned error, restarting",
                            self.factories[srv.factory].name(Token(idx))
                        );
                        failed = Some((Token(idx), srv.factory));
                        srv.status = WorkerServiceStatus::Failed;
                    }
                }
            }
        }
        if let Some(idx) = failed {
            Err(idx)
        } else {
            Ok(ready)
        }
    }
}

enum WorkerState {
    Available,
    Unavailable,
    Restarting(
        usize,
        Token,
        BoxFuture<'static, Result<Vec<(Token, BoxedServerService)>, ()>>,
    ),
    Shutdown(Sleep, Sleep, Option<oneshot::Sender<bool>>),
}

impl Future for Worker {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // `StopWorker` message handler
        let stop = Pin::new(&mut self.rx2).poll_next(cx);
        if let Poll::Ready(Some(StopCommand {
            graceful,
            mut result,
        })) = stop
        {
            self.availability.set(false);
            let num = num_connections();
            if num == 0 {
                info!("Shutting down worker, 0 connections");
                let _ = result.send(true);
                return Poll::Ready(());
            } else if graceful {
                self.shutdown(false);
                let num = num_connections();
                if num != 0 {
                    info!("Graceful worker shutdown, {} connections", num);
                    self.state = WorkerState::Shutdown(
                        sleep(STOP_TIMEOUT),
                        sleep(self.shutdown_timeout),
                        Some(result),
                    );
                } else {
                    let _ = result.send(true);
                    return Poll::Ready(());
                }
            } else {
                info!("Force shutdown worker, {} connections", num);
                self.shutdown(true);
                let _ = result.send(false);
                return Poll::Ready(());
            }
        }

        match self.state {
            WorkerState::Unavailable => {
                match self.check_readiness(cx) {
                    Ok(true) => {
                        // process requests from wait queue
                        self.state = WorkerState::Available;
                        self.availability.set(true);
                        self.poll(cx)
                    }
                    Ok(false) => Poll::Pending,
                    Err((token, idx)) => {
                        trace!(
                            "Service {:?} failed, restarting",
                            self.factories[idx].name(token)
                        );
                        self.services[token.0].status = WorkerServiceStatus::Restarting;
                        self.state = WorkerState::Restarting(
                            idx,
                            token,
                            self.factories[idx].create(),
                        );
                        self.poll(cx)
                    }
                }
            }
            WorkerState::Restarting(idx, token, ref mut fut) => {
                match Pin::new(fut).poll(cx) {
                    Poll::Ready(Ok(item)) => {
                        // TODO: deal with multiple services
                        if let Some((token, service)) = item.into_iter().next() {
                            trace!(
                                "Service {:?} has been restarted",
                                self.factories[idx].name(token)
                            );
                            self.services[token.0].created(service);
                            // service is restarted, now wait for readiness
                            self.state = WorkerState::Unavailable;
                            return self.poll(cx);
                        }
                    }
                    Poll::Ready(Err(_)) => {
                        panic!(
                            "Cannot restart {:?} service",
                            self.factories[idx].name(token)
                        );
                    }
                    Poll::Pending => return Poll::Pending,
                }
                self.poll(cx)
            }
            WorkerState::Shutdown(ref mut t1, ref mut t2, ref mut tx) => {
                let num = num_connections();
                if num == 0 {
                    let _ = tx.take().unwrap().send(true);
                    Arbiter::current().stop();
                    return Poll::Ready(());
                }

                // check graceful timeout
                match t2.poll_elapsed(cx) {
                    Poll::Pending => (),
                    Poll::Ready(_) => {
                        let _ = tx.take().unwrap().send(false);
                        self.shutdown(true);
                        Arbiter::current().stop();
                        return Poll::Ready(());
                    }
                }

                // sleep for 1 second and then check again
                match t1.poll_elapsed(cx) {
                    Poll::Pending => (),
                    Poll::Ready(_) => {
                        *t1 = sleep(STOP_TIMEOUT);
                        let _ = t1.poll_elapsed(cx);
                    }
                }
                Poll::Pending
            }
            WorkerState::Available => {
                loop {
                    match self.check_readiness(cx) {
                        Ok(true) => (),
                        Ok(false) => {
                            trace!("Worker is unavailable");
                            self.availability.set(false);
                            self.state = WorkerState::Unavailable;
                            return self.poll(cx);
                        }
                        Err((token, idx)) => {
                            trace!(
                                "Service {:?} failed, restarting",
                                self.factories[idx].name(token)
                            );
                            self.availability.set(false);
                            self.services[token.0].status = WorkerServiceStatus::Restarting;
                            self.state = WorkerState::Restarting(
                                idx,
                                token,
                                self.factories[idx].create(),
                            );
                            return self.poll(cx);
                        }
                    }

                    let next = ready!(Pin::new(&mut self.rx).poll_next(cx));
                    if let Some(WorkerCommand(msg)) = next {
                        // handle incoming io stream
                        let guard = self.conns.get();
                        let srv = &self.services[msg.token.0];

                        if log::log_enabled!(log::Level::Trace) {
                            trace!(
                                "Got socket for service: {:?}",
                                self.factories[srv.factory].name(msg.token)
                            );
                        }
                        let srv = srv.service.clone();
                        spawn(async move {
                            let _ = srv
                                .call((Some(guard), ServerMessage::Connect(msg.io)))
                                .await;
                        });
                    } else {
                        return Poll::Ready(());
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::io::Io;
    use crate::server::service::Factory;
    use crate::service::{Service, ServiceCtx, ServiceFactory};
    use crate::util::{lazy, Ready};

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
        type Future<'f> = Ready<Srv, ()>;

        fn create(&self, _: ()) -> Self::Future<'_> {
            let mut cnt = self.counter.lock().unwrap();
            *cnt += 1;
            Ready::Ok(Srv {
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
        type Future<'f> = Ready<(), ()>;

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

        fn call<'a>(&'a self, _: Io, _: ServiceCtx<'a, Self>) -> Self::Future<'a> {
            Ready::Ok(())
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
        let avail =
            WorkerAvailability::new(AcceptNotify::new(waker.clone(), sync_tx.clone()));

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

        let (tx, rx) = oneshot::oneshot();
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
        let avail = WorkerAvailability::new(AcceptNotify::new(waker, sync_tx.clone()));
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

        let (tx, rx) = oneshot::oneshot();
        tx2.try_send(StopCommand {
            graceful: false,
            result: tx,
        })
        .unwrap();

        assert!(lazy(|cx| Pin::new(&mut worker).poll(cx)).await.is_ready());
        let _ = rx.await;
    }
}
