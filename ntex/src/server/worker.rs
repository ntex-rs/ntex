use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time;

use futures::channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use futures::channel::oneshot;
use futures::future::{join_all, LocalBoxFuture, MapOk};
use futures::{Future, FutureExt, Stream, TryFutureExt};

use crate::rt::time::{delay_until, Delay, Instant};
use crate::rt::{spawn, Arbiter};
use crate::util::counter::Counter;

use super::accept::AcceptNotify;
use super::service::{BoxedServerService, InternalServiceFactory, ServerMessage};
use super::socket::{SocketAddr, StdStream};
use super::Token;

pub(super) struct WorkerCommand(Conn);

/// Stop worker message. Returns `true` on successful shutdown
/// and `false` if some connections still alive.
pub(super) struct StopCommand {
    graceful: bool,
    result: oneshot::Sender<bool>,
}

#[derive(Debug)]
pub(super) struct Conn {
    pub(super) io: StdStream,
    pub(super) token: Token,
    pub(super) peer: Option<SocketAddr>,
}

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

#[derive(Clone)]
pub(super) struct WorkerClient {
    pub(super) idx: usize,
    tx1: UnboundedSender<WorkerCommand>,
    tx2: UnboundedSender<StopCommand>,
    avail: WorkerAvailability,
}

impl WorkerClient {
    pub(super) fn new(
        idx: usize,
        tx1: UnboundedSender<WorkerCommand>,
        tx2: UnboundedSender<StopCommand>,
        avail: WorkerAvailability,
    ) -> Self {
        WorkerClient {
            idx,
            tx1,
            tx2,
            avail,
        }
    }

    pub(super) fn send(&self, msg: Conn) -> Result<(), Conn> {
        self.tx1
            .unbounded_send(WorkerCommand(msg))
            .map_err(|msg| msg.into_inner().0)
    }

    pub(super) fn available(&self) -> bool {
        self.avail.available()
    }

    pub(super) fn stop(&self, graceful: bool) -> oneshot::Receiver<bool> {
        let (result, rx) = oneshot::channel();
        let _ = self.tx2.unbounded_send(StopCommand { graceful, result });
        rx
    }
}

#[derive(Clone)]
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
            self.notify.notify()
        }
    }
}

/// Service worker
///
/// Worker accepts Socket objects via unbounded channel and starts stream
/// processing.
pub(super) struct Worker {
    rx: UnboundedReceiver<WorkerCommand>,
    rx2: UnboundedReceiver<StopCommand>,
    services: Vec<WorkerService>,
    availability: WorkerAvailability,
    conns: Counter,
    factories: Vec<Box<dyn InternalServiceFactory>>,
    state: WorkerState,
    shutdown_timeout: time::Duration,
}

struct WorkerService {
    factory: usize,
    status: WorkerServiceStatus,
    service: BoxedServerService,
}

impl WorkerService {
    fn created(&mut self, service: BoxedServerService) {
        self.service = service;
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
        shutdown_timeout: time::Duration,
    ) -> WorkerClient {
        let (tx1, rx1) = unbounded();
        let (tx2, rx2) = unbounded();
        let avail = availability.clone();

        Arbiter::default().exec_fn(move || {
            let _ = spawn(async move {
                match Worker::create(rx1, rx2, factories, availability, shutdown_timeout)
                    .await
                {
                    Ok(wrk) => {
                        let _ = spawn(wrk);
                    }
                    Err(e) => {
                        error!("Can not start worker: {:?}", e);
                        Arbiter::current().stop();
                    }
                }
            });
        });

        WorkerClient::new(idx, tx1, tx2, avail)
    }

    async fn create(
        rx: UnboundedReceiver<WorkerCommand>,
        rx2: UnboundedReceiver<StopCommand>,
        factories: Vec<Box<dyn InternalServiceFactory>>,
        availability: WorkerAvailability,
        shutdown_timeout: time::Duration,
    ) -> Result<Worker, ()> {
        availability.set(false);
        let mut wrk = MAX_CONNS_COUNTER.with(move |conns| Worker {
            rx,
            rx2,
            availability,
            factories,
            shutdown_timeout,
            services: Vec::new(),
            conns: conns.clone(),
            state: WorkerState::Unavailable,
        });

        let mut fut: Vec<MapOk<LocalBoxFuture<'static, _>, _>> = Vec::new();
        for (idx, factory) in wrk.factories.iter().enumerate() {
            fut.push(factory.create().map_ok(move |r| {
                r.into_iter()
                    .map(|(t, s): (Token, _)| (idx, t, s))
                    .collect::<Vec<_>>()
            }));
        }

        let res = join_all(fut).await;
        let res: Result<Vec<_>, _> = res.into_iter().collect();
        match res {
            Ok(services) => {
                for item in services {
                    for (factory, token, service) in item {
                        assert_eq!(token.0, wrk.services.len());
                        wrk.services.push(WorkerService {
                            factory,
                            service,
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
                    spawn(
                        srv.service
                            .call((None, ServerMessage::ForceShutdown))
                            .map(|_| ()),
                    );
                }
            });
        } else {
            let timeout = self.shutdown_timeout;
            self.services.iter_mut().for_each(move |srv| {
                if srv.status == WorkerServiceStatus::Available {
                    srv.status = WorkerServiceStatus::Stopping;
                    spawn(
                        srv.service
                            .call((None, ServerMessage::Shutdown(timeout)))
                            .map(|_| ()),
                    );
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
        Pin<Box<dyn Future<Output = Result<Vec<(Token, BoxedServerService)>, ()>>>>,
    ),
    Shutdown(
        Pin<Box<Delay>>,
        Pin<Box<Delay>>,
        Option<oneshot::Sender<bool>>,
    ),
}

impl Future for Worker {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // `StopWorker` message handler
        if let Poll::Ready(Some(StopCommand { graceful, result })) =
            Pin::new(&mut self.rx2).poll_next(cx)
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
                        Box::pin(delay_until(
                            Instant::now() + time::Duration::from_secs(1),
                        )),
                        Box::pin(delay_until(Instant::now() + self.shutdown_timeout)),
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
                            "Can not restart {:?} service",
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
                match t2.as_mut().poll(cx) {
                    Poll::Pending => (),
                    Poll::Ready(_) => {
                        let _ = tx.take().unwrap().send(false);
                        self.shutdown(true);
                        Arbiter::current().stop();
                        return Poll::Ready(());
                    }
                }

                // sleep for 1 second and then check again
                match t1.as_mut().poll(cx) {
                    Poll::Pending => (),
                    Poll::Ready(_) => {
                        *t1 = Box::pin(delay_until(
                            Instant::now() + time::Duration::from_secs(1),
                        ));
                        let _ = t1.as_mut().poll(cx);
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
                            self.services[token.0].status =
                                WorkerServiceStatus::Restarting;
                            self.state = WorkerState::Restarting(
                                idx,
                                token,
                                self.factories[idx].create(),
                            );
                            return self.poll(cx);
                        }
                    }

                    match Pin::new(&mut self.rx).poll_next(cx) {
                        // handle incoming io stream
                        Poll::Ready(Some(WorkerCommand(msg))) => {
                            let guard = self.conns.get();
                            let srv = &self.services[msg.token.0];

                            if log::log_enabled!(log::Level::Trace) {
                                trace!(
                                    "Got socket for service: {:?}",
                                    self.factories[srv.factory].name(msg.token)
                                );
                            }
                            let _ = srv
                                .service
                                .call((Some(guard), ServerMessage::Connect(msg.io)));
                        }
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(None) => return Poll::Ready(()),
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use futures::future::{lazy, ok, Ready};
    use futures::SinkExt;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::rt::net::TcpStream;
    use crate::server::service::Factory;
    use crate::service::{Service, ServiceFactory};

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

    impl ServiceFactory for SrvFactory {
        type Request = TcpStream;
        type Response = ();
        type Error = ();
        type Service = Srv;
        type Config = ();
        type InitError = ();
        type Future = Ready<Result<Srv, ()>>;

        fn new_service(&self, _: ()) -> Self::Future {
            let mut cnt = self.counter.lock().unwrap();
            *cnt += 1;
            ok(Srv {
                st: self.st.clone(),
            })
        }
    }

    struct Srv {
        st: Arc<Mutex<St>>,
    }

    impl Service for Srv {
        type Request = TcpStream;
        type Response = ();
        type Error = ();
        type Future = Ready<Result<(), ()>>;

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

        fn poll_shutdown(&self, _: &mut Context<'_>, _: bool) -> Poll<()> {
            match *self.st.lock().unwrap() {
                St::Ready => Poll::Ready(()),
                St::Fail | St::Pending => Poll::Pending,
            }
        }

        fn call(&self, _: TcpStream) -> Self::Future {
            ok(())
        }
    }

    #[ntex_rt::test]
    #[allow(clippy::mutex_atomic)]
    async fn basics() {
        let (_tx1, rx1) = unbounded();
        let (mut tx2, rx2) = unbounded();
        let avail = WorkerAvailability::new(AcceptNotify::default());

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
                move || f.clone(),
                "127.0.0.1:8080".parse().unwrap(),
            )],
            avail.clone(),
            time::Duration::from_secs(5),
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
        tx2.send(StopCommand {
            graceful: true,
            result: tx,
        })
        .await
        .unwrap();

        let _ = lazy(|cx| Pin::new(&mut worker).poll(cx)).await;
        assert!(!avail.available());
        drop(g);
        assert!(lazy(|cx| Pin::new(&mut worker).poll(cx)).await.is_ready());
        let _ = rx.await;

        // force shutdown
        let (_tx1, rx1) = unbounded();
        let (mut tx2, rx2) = unbounded();
        let avail = WorkerAvailability::new(AcceptNotify::default());
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
                move || f.clone(),
                "127.0.0.1:8080".parse().unwrap(),
            )],
            avail.clone(),
            time::Duration::from_secs(5),
        )
        .await
        .unwrap();

        // shutdown
        let _g = MAX_CONNS_COUNTER.with(|conns| conns.get());

        *st.lock().unwrap() = St::Ready;
        let _ = lazy(|cx| Pin::new(&mut worker).poll(cx)).await;
        assert!(avail.available());

        let (tx, rx) = oneshot::channel();
        tx2.send(StopCommand {
            graceful: false,
            result: tx,
        })
        .await
        .unwrap();

        assert!(lazy(|cx| Pin::new(&mut worker).poll(cx)).await.is_ready());
        let _ = rx.await;
    }
}
