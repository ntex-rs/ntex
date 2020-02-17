use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time;

use actix_rt::time::{delay_until, Delay, Instant};
use actix_rt::{spawn, Arbiter};
use actix_utils::counter::Counter;
use futures::channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use futures::channel::oneshot;
use futures::future::{join_all, LocalBoxFuture, MapOk};
use futures::{Future, FutureExt, Stream, TryFutureExt};
use log::{error, info, trace};

use crate::accept::AcceptNotify;
use crate::service::{BoxedServerService, InternalServiceFactory, ServerMessage};
use crate::socket::{SocketAddr, StdStream};
use crate::Token;

pub(crate) struct WorkerCommand(Conn);

/// Stop worker message. Returns `true` on successful shutdown
/// and `false` if some connections still alive.
pub(crate) struct StopCommand {
    graceful: bool,
    result: oneshot::Sender<bool>,
}

#[derive(Debug)]
pub(crate) struct Conn {
    pub io: StdStream,
    pub token: Token,
    pub peer: Option<SocketAddr>,
}

static MAX_CONNS: AtomicUsize = AtomicUsize::new(25600);

/// Sets the maximum per-worker number of concurrent connections.
///
/// All socket listeners will stop accepting connections when this limit is
/// reached for each worker.
///
/// By default max connections is set to a 25k per worker.
pub fn max_concurrent_connections(num: usize) {
    MAX_CONNS.store(num, Ordering::Relaxed);
}

pub(crate) fn num_connections() -> usize {
    MAX_CONNS_COUNTER.with(|conns| conns.total())
}

thread_local! {
    static MAX_CONNS_COUNTER: Counter =
        Counter::new(MAX_CONNS.load(Ordering::Relaxed));
}

#[derive(Clone)]
pub(crate) struct WorkerClient {
    pub idx: usize,
    tx1: UnboundedSender<WorkerCommand>,
    tx2: UnboundedSender<StopCommand>,
    avail: WorkerAvailability,
}

impl WorkerClient {
    pub fn new(
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

    pub fn send(&self, msg: Conn) -> Result<(), Conn> {
        self.tx1
            .unbounded_send(WorkerCommand(msg))
            .map_err(|msg| msg.into_inner().0)
    }

    pub fn available(&self) -> bool {
        self.avail.available()
    }

    pub fn stop(&self, graceful: bool) -> oneshot::Receiver<bool> {
        let (result, rx) = oneshot::channel();
        let _ = self.tx2.unbounded_send(StopCommand { graceful, result });
        rx
    }
}

#[derive(Clone)]
pub(crate) struct WorkerAvailability {
    notify: AcceptNotify,
    available: Arc<AtomicBool>,
}

impl WorkerAvailability {
    pub fn new(notify: AcceptNotify) -> Self {
        WorkerAvailability {
            notify,
            available: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn available(&self) -> bool {
        self.available.load(Ordering::Acquire)
    }

    pub fn set(&self, val: bool) {
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
pub(crate) struct Worker {
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
    pub(crate) fn start(
        idx: usize,
        factories: Vec<Box<dyn InternalServiceFactory>>,
        availability: WorkerAvailability,
        shutdown_timeout: time::Duration,
    ) -> WorkerClient {
        let (tx1, rx) = unbounded();
        let (tx2, rx2) = unbounded();
        let avail = availability.clone();

        Arbiter::new().send(
            async move {
                availability.set(false);
                let mut wrk = MAX_CONNS_COUNTER.with(move |conns| Worker {
                    rx,
                    rx2,
                    availability,
                    factories,
                    shutdown_timeout,
                    services: Vec::new(),
                    conns: conns.clone(),
                    state: WorkerState::Unavailable(Vec::new()),
                });

                let mut fut: Vec<MapOk<LocalBoxFuture<'static, _>, _>> = Vec::new();
                for (idx, factory) in wrk.factories.iter().enumerate() {
                    fut.push(factory.create().map_ok(move |r| {
                        r.into_iter()
                            .map(|(t, s): (Token, _)| (idx, t, s))
                            .collect::<Vec<_>>()
                    }));
                }

                spawn(async move {
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
                        }
                        Err(e) => {
                            error!("Can not start worker: {:?}", e);
                            Arbiter::current().stop();
                        }
                    }
                    wrk.await
                });
            }
            .boxed(),
        );

        WorkerClient::new(idx, tx1, tx2, avail)
    }

    fn shutdown(&mut self, force: bool) {
        if force {
            self.services.iter_mut().for_each(|srv| {
                if srv.status == WorkerServiceStatus::Available {
                    srv.status = WorkerServiceStatus::Stopped;
                    actix_rt::spawn(
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
                    actix_rt::spawn(
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
    Unavailable(Vec<Conn>),
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
                        Box::pin(delay_until(Instant::now() + time::Duration::from_secs(1))),
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
            WorkerState::Unavailable(ref mut conns) => {
                let conn = conns.pop();
                match self.check_readiness(cx) {
                    Ok(true) => {
                        // process requests from wait queue
                        if let Some(conn) = conn {
                            let guard = self.conns.get();
                            let _ = self.services[conn.token.0]
                                .service
                                .call((Some(guard), ServerMessage::Connect(conn.io)));
                        } else {
                            self.state = WorkerState::Available;
                            self.availability.set(true);
                        }
                        self.poll(cx)
                    }
                    Ok(false) => {
                        // push connection back to queue
                        if let Some(conn) = conn {
                            match self.state {
                                WorkerState::Unavailable(ref mut conns) => {
                                    conns.push(conn);
                                }
                                _ => (),
                            }
                        }
                        Poll::Pending
                    }
                    Err((token, idx)) => {
                        trace!(
                            "Service {:?} failed, restarting",
                            self.factories[idx].name(token)
                        );
                        self.services[token.0].status = WorkerServiceStatus::Restarting;
                        self.state =
                            WorkerState::Restarting(idx, token, self.factories[idx].create());
                        self.poll(cx)
                    }
                }
            }
            WorkerState::Restarting(idx, token, ref mut fut) => {
                match Pin::new(fut).poll(cx) {
                    Poll::Ready(Ok(item)) => {
                        for (token, service) in item {
                            trace!(
                                "Service {:?} has been restarted",
                                self.factories[idx].name(token)
                            );
                            self.services[token.0].created(service);
                            self.state = WorkerState::Unavailable(Vec::new());
                            return self.poll(cx);
                        }
                    }
                    Poll::Ready(Err(_)) => {
                        panic!(
                            "Can not restart {:?} service",
                            self.factories[idx].name(token)
                        );
                    }
                    Poll::Pending => {
                        return Poll::Pending;
                    }
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
                    match Pin::new(&mut self.rx).poll_next(cx) {
                        // handle incoming io stream
                        Poll::Ready(Some(WorkerCommand(msg))) => {
                            match self.check_readiness(cx) {
                                Ok(true) => {
                                    let guard = self.conns.get();
                                    let _ = self.services[msg.token.0]
                                        .service
                                        .call((Some(guard), ServerMessage::Connect(msg.io)));
                                    continue;
                                }
                                Ok(false) => {
                                    trace!("Worker is unavailable");
                                    self.availability.set(false);
                                    self.state = WorkerState::Unavailable(vec![msg]);
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
                                }
                            }
                            return self.poll(cx);
                        }
                        Poll::Pending => {
                            self.state = WorkerState::Available;
                            return Poll::Pending;
                        }
                        Poll::Ready(None) => return Poll::Ready(()),
                    }
                }
            }
        }
    }
}
