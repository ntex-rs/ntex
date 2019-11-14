use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::{mem, time};

use actix_rt::{spawn, Arbiter};
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
use futures::channel::oneshot;
use futures::future::{join_all, LocalBoxFuture, MapOk};
use futures::{Future, FutureExt, Stream, TryFutureExt};
use log::{error, info, trace};
use tokio_timer::{delay, Delay};

use crate::accept::AcceptNotify;
use crate::counter::Counter;
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
    services: Vec<Option<(usize, BoxedServerService)>>,
    availability: WorkerAvailability,
    conns: Counter,
    factories: Vec<Box<dyn InternalServiceFactory>>,
    state: WorkerState,
    shutdown_timeout: time::Duration,
}

impl Worker {
    pub(crate) fn start(
        rx: UnboundedReceiver<WorkerCommand>,
        rx2: UnboundedReceiver<StopCommand>,
        factories: Vec<Box<dyn InternalServiceFactory>>,
        availability: WorkerAvailability,
        shutdown_timeout: time::Duration,
    ) {
        availability.set(false);
        let mut wrk = MAX_CONNS_COUNTER.with(|conns| Worker {
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

        spawn(
            async move {
                let res = join_all(fut).await;
                let res: Result<Vec<_>, _> = res.into_iter().collect();
                match res {
                    Ok(services) => {
                        for item in services {
                            for (idx, token, service) in item {
                                while token.0 >= wrk.services.len() {
                                    wrk.services.push(None);
                                }
                                wrk.services[token.0] = Some((idx, service));
                            }
                        }
                    }
                    Err(e) => {
                        error!("Can not start worker: {:?}", e);
                        Arbiter::current().stop();
                    }
                }
                wrk.await
            }
            .boxed_local(),
        );
    }

    fn shutdown(&mut self, force: bool) {
        if force {
            self.services.iter_mut().for_each(|h| {
                if let Some(h) = h {
                    let _ = h.1.call((None, ServerMessage::ForceShutdown));
                }
            });
        } else {
            let timeout = self.shutdown_timeout;
            self.services.iter_mut().for_each(move |h| {
                if let Some(h) = h {
                    let _ = h.1.call((None, ServerMessage::Shutdown(timeout)));
                }
            });
        }
    }

    fn check_readiness(
        &mut self,
        trace: bool,
        cx: &mut Context<'_>,
    ) -> Result<bool, (Token, usize)> {
        let mut ready = self.conns.available(cx);
        let mut failed = None;
        for (token, service) in &mut self.services.iter_mut().enumerate() {
            if let Some(service) = service {
                match service.1.poll_ready(cx) {
                    Poll::Ready(Ok(_)) => {
                        if trace {
                            trace!(
                                "Service {:?} is available",
                                self.factories[service.0].name(Token(token))
                            );
                        }
                    }
                    Poll::Pending => ready = false,
                    Poll::Ready(Err(_)) => {
                        error!(
                            "Service {:?} readiness check returned error, restarting",
                            self.factories[service.0].name(Token(token))
                        );
                        failed = Some((Token(token), service.0));
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
    None,
    Available,
    Unavailable(Vec<Conn>),
    Restarting(
        usize,
        Token,
        Pin<Box<dyn Future<Output = Result<Vec<(Token, BoxedServerService)>, ()>>>>,
    ),
    Shutdown(Delay, Delay, oneshot::Sender<bool>),
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
                        delay(time::Instant::now() + time::Duration::from_secs(1)),
                        delay(time::Instant::now() + self.shutdown_timeout),
                        result,
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

        let state = mem::replace(&mut self.state, WorkerState::None);

        match state {
            WorkerState::Unavailable(mut conns) => {
                match self.check_readiness(true, cx) {
                    Ok(true) => {
                        self.state = WorkerState::Available;

                        // process requests from wait queue
                        while let Some(msg) = conns.pop() {
                            match self.check_readiness(false, cx) {
                                Ok(true) => {
                                    let guard = self.conns.get();
                                    let _ = self.services[msg.token.0]
                                        .as_mut()
                                        .expect("actix net bug")
                                        .1
                                        .call((Some(guard), ServerMessage::Connect(msg.io)));
                                }
                                Ok(false) => {
                                    trace!("Worker is unavailable");
                                    self.state = WorkerState::Unavailable(conns);
                                    return self.poll(cx);
                                }
                                Err((token, idx)) => {
                                    trace!(
                                        "Service {:?} failed, restarting",
                                        self.factories[idx].name(token)
                                    );
                                    self.state = WorkerState::Restarting(
                                        idx,
                                        token,
                                        self.factories[idx].create(),
                                    );
                                    return self.poll(cx);
                                }
                            }
                        }
                        self.availability.set(true);
                        return self.poll(cx);
                    }
                    Ok(false) => {
                        self.state = WorkerState::Unavailable(conns);
                        return Poll::Pending;
                    }
                    Err((token, idx)) => {
                        trace!(
                            "Service {:?} failed, restarting",
                            self.factories[idx].name(token)
                        );
                        self.state =
                            WorkerState::Restarting(idx, token, self.factories[idx].create());
                        return self.poll(cx);
                    }
                }
            }
            WorkerState::Restarting(idx, token, mut fut) => {
                match Pin::new(&mut fut).poll(cx) {
                    Poll::Ready(Ok(item)) => {
                        for (token, service) in item {
                            trace!(
                                "Service {:?} has been restarted",
                                self.factories[idx].name(token)
                            );
                            self.services[token.0] = Some((idx, service));
                            self.state = WorkerState::Unavailable(Vec::new());
                        }
                    }
                    Poll::Ready(Err(_)) => {
                        panic!(
                            "Can not restart {:?} service",
                            self.factories[idx].name(token)
                        );
                    }
                    Poll::Pending => {
                        self.state = WorkerState::Restarting(idx, token, fut);
                        return Poll::Pending;
                    }
                }
                return self.poll(cx);
            }
            WorkerState::Shutdown(mut t1, mut t2, tx) => {
                let num = num_connections();
                if num == 0 {
                    let _ = tx.send(true);
                    Arbiter::current().stop();
                    return Poll::Ready(());
                }

                // check graceful timeout
                match Pin::new(&mut t2).poll(cx) {
                    Poll::Pending => (),
                    Poll::Ready(_) => {
                        self.shutdown(true);
                        let _ = tx.send(false);
                        Arbiter::current().stop();
                        return Poll::Ready(());
                    }
                }

                // sleep for 1 second and then check again
                match Pin::new(&mut t1).poll(cx) {
                    Poll::Pending => (),
                    Poll::Ready(_) => {
                        t1 = delay(time::Instant::now() + time::Duration::from_secs(1));
                        let _ = Pin::new(&mut t1).poll(cx);
                    }
                }
                self.state = WorkerState::Shutdown(t1, t2, tx);
                return Poll::Pending;
            }
            WorkerState::Available => {
                loop {
                    match Pin::new(&mut self.rx).poll_next(cx) {
                        // handle incoming tcp stream
                        Poll::Ready(Some(WorkerCommand(msg))) => {
                            match self.check_readiness(false, cx) {
                                Ok(true) => {
                                    let guard = self.conns.get();
                                    let _ = self.services[msg.token.0]
                                        .as_mut()
                                        .expect("actix-server bug")
                                        .1
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
            WorkerState::None => panic!(),
        };
    }
}
