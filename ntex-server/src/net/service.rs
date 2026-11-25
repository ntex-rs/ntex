use std::task::{Context, Poll, Wake, Waker};
use std::{cell::RefCell, collections::VecDeque, fmt, future::poll_fn, io, pin::Pin, sync::Arc};

use ntex_io::{IoConfig, IoMetricsGroup};
use ntex_service::{BindCtx, Ctx, ReadyNotifier, Service, cfg::SharedCfg};
use ntex_util::future::{Stream, join_all};
use ntex_util::{HashMap, services::Counter, services::counter::CounterGuard};

use crate::{ServerAppConfig, ServerConfiguration, metrics::WorkerSlot};

use super::accept::{AcceptNotify, AcceptorCommand};
use super::factory::{FactoryServiceType, NetService};
use super::reuseport::Reuseport;
use super::{Token, socket::Connection};

/// Worker configuration for network servers.
///
/// Created by [`ServerBuilder::run`](super::ServerBuilder::run). Each worker
/// creates its application state and the services for all registered
/// listeners.
pub struct StreamServer<Cfg> {
    accept: AcceptNotify,
    state: Arc<Cfg>,
    services: Vec<FactoryServiceType<Cfg>>,
    max_conns: usize,
    reuseport: Option<Arc<Reuseport>>,
}

impl<Cfg: ServerAppConfig> StreamServer<Cfg> {
    pub(crate) fn new(
        accept: AcceptNotify,
        state: Arc<Cfg>,
        services: Vec<FactoryServiceType<Cfg>>,
        max_conns: usize,
        reuseport: Option<Arc<Reuseport>>,
    ) -> Self {
        Self {
            accept,
            state,
            services,
            max_conns,
            reuseport,
        }
    }
}

impl<Cfg> fmt::Debug for StreamServer<Cfg> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamServer")
            .field("services", &self.services.len())
            .field("max_conns", &self.max_conns)
            .field("reuseport", &self.reuseport)
            .finish()
    }
}

/// Worker service factory.
impl<Cfg: ServerAppConfig> ServerConfiguration for StreamServer<Cfg> {
    type Item = Connection;
    type Service = StreamService;

    /// Create service for handling connections
    async fn create(&self) -> io::Result<Self::Service> {
        // construct configuration
        let cfg = self.state.create().await?;

        // construct services
        let mut tokens = HashMap::default();
        let mut services = Vec::new();
        let group = IoMetricsGroup::current();

        for info in &self.services {
            for (svc, _, svc_tokens) in info.create(cfg.clone()).await.map_err(io::Error::other)? {
                let idx = services.len();
                services.push(ListenerService {
                    svc,
                    pending: RefCell::default(),
                });
                for (token, cfg) in &svc_tokens {
                    // listener connections have their own metrics
                    group.register(&cfg.get::<IoConfig>());
                    tokens.insert(*token, (idx, cfg.clone()));
                }
            }
        }

        let readiness = ReadyNotifier::new();
        let check_waker = (services.len() > 1).then(|| readiness_waker(readiness.clone()));
        Ok(StreamService {
            services,
            tokens,
            conns: Counter::new(self.max_conns),
            metrics: WorkerSlot::current(),
            readiness,
            check_waker,
        })
    }

    /// Pause the server.
    fn pause(&self) {
        if let Some(ref rp) = self.reuseport {
            rp.pause();
        }
        self.accept.send(AcceptorCommand::Pause);
    }

    /// Resume the server.
    fn resume(&self) {
        if let Some(ref rp) = self.reuseport {
            rp.resume();
        }
        self.accept.send(AcceptorCommand::Resume);
    }

    /// Terminate the server.
    fn terminate(&self) {
        if let Some(ref rp) = self.reuseport {
            rp.stop();
        }
        self.accept.send(AcceptorCommand::Terminate);
    }

    /// Stop the server.
    async fn stop(&self) {
        if let Some(ref rp) = self.reuseport {
            rp.stop();
        }
        let (tx, rx) = oneshot::channel();
        self.accept.send(AcceptorCommand::Stop(tx));
        let _ = rx.await;
    }

    fn incoming_cpu(&self, con: &Connection) -> Option<usize> {
        con.incoming_cpu()
    }

    /// Accept connections on the worker's own listeners.
    fn local_items(&self) -> Option<Pin<Box<dyn Stream<Item = Connection>>>> {
        let local = self.reuseport.as_ref()?.local()?;
        Some(Box::pin(local))
    }
}

impl<Cfg: ServerAppConfig> Clone for StreamServer<Cfg> {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
            accept: self.accept.clone(),
            services: self.services.iter().map(|s| s.clo()).collect(),
            max_conns: self.max_conns,
            reuseport: self.reuseport.clone(),
        }
    }
}

/// Per-worker service that dispatches connections to listener services.
pub struct StreamService {
    tokens: HashMap<Token, (usize, SharedCfg)>,
    services: Vec<ListenerService>,
    conns: Counter,
    metrics: Option<Arc<WorkerSlot>>,
    readiness: ReadyNotifier,
    check_waker: Option<Waker>,
}

type PendingConnection = (Connection, SharedCfg, ConnGuard);

struct ListenerService {
    svc: Box<dyn NetService>,
    pending: RefCell<VecDeque<PendingConnection>>,
}

impl ListenerService {
    fn dispatch(&self, (con, cfg, guard): PendingConnection) -> Result<(), ()> {
        let stream = con.io.convert(con.nodelay, cfg).map_err(|e| {
            log::error!("Cannot convert to an async io stream: {e}");
        })?;
        self.svc.call(stream, guard);
        Ok(())
    }
}

struct ReadinessWake(async_channel::Sender<()>);

impl Wake for ReadinessWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        let _ = self.0.try_send(());
    }
}

fn readiness_waker(notifier: ReadyNotifier) -> Waker {
    // A listener can wake while the worker's aggregate readiness is still cached.
    let (tx, rx) = async_channel::bounded(1);
    ntex_rt::spawn_detached(async move {
        while rx.recv().await.is_ok() {
            notifier.notify();
        }
    });
    Waker::from(Arc::new(ReadinessWake(tx)))
}

impl StreamService {
    fn cancel_pending(&self) {
        for service in &self.services {
            service.pending.borrow_mut().clear();
        }
    }
}

/// Open connection, released on drop.
pub(crate) struct ConnGuard {
    _guard: CounterGuard,
    metrics: Option<Arc<WorkerSlot>>,
}

impl ConnGuard {
    fn new(guard: CounterGuard, metrics: Option<Arc<WorkerSlot>>) -> Self {
        if let Some(ref m) = metrics {
            m.conn_opened();
        }
        Self {
            _guard: guard,
            metrics,
        }
    }
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        if let Some(ref m) = self.metrics {
            m.conn_closed();
        }
    }
}

impl fmt::Debug for StreamService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamService")
            .field("tokens", &self.tokens)
            .field("conns", &self.conns)
            .finish()
    }
}

impl Service<(), Connection> for StreamService {
    type Res = ();
    type Error = ();

    fn bind(&self, ctx: BindCtx<'_>) {
        self.conns.bind(&ctx);
        ctx.subscribe(&self.readiness);
        for service in &self.services {
            service.svc.bind(&ctx);
        }
    }

    async fn ready(&self, _: Ctx<'_, Self, ()>) -> Result<(), Self::Error> {
        poll_fn(|cx| {
            let cx = &mut Context::from_waker(self.check_waker.as_ref().unwrap_or(cx.waker()));
            let mut available = self.services.is_empty();
            for (idx, service) in self.services.iter().enumerate() {
                match service.svc.poll_ready(cx) {
                    Poll::Ready(Ok(())) => available = true,
                    Poll::Ready(Err(())) => {
                        if let Some((_, cfg)) = self.tokens.values().find(|(i, _)| *i == idx) {
                            log::error!("{}: Service readiness has failed", cfg.tag());
                        }
                        self.cancel_pending();
                        return Poll::Ready(Err(()));
                    }
                    Poll::Pending => {}
                }
            }

            // Buffered sockets already hold permits, so dispatch even at the limit.
            for service in &self.services {
                if service.svc.is_ready() {
                    let item = service.pending.borrow_mut().pop_front();
                    if let Some(item) = item {
                        let _ = service.dispatch(item);
                        if !service.pending.borrow().is_empty() {
                            cx.waker().wake_by_ref();
                        }
                    }
                }
            }

            if available && self.conns.is_available() {
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            }
        })
        .await
    }

    async fn shutdown(&self, _: Ctx<'_, Self, ()>) {
        self.cancel_pending();
        let _ = join_all(self.services.iter().map(|s| s.svc.shutdown())).await;
        log::info!(
            "Worker service shutdown, {} connections",
            self.conns.total()
        );
    }

    #[allow(clippy::unused_async_trait_impl)]
    async fn call(&self, con: Connection, _: Ctx<'_, Self, ()>) -> Result<(), ()> {
        if let Some((idx, cfg)) = self.tokens.get(&con.token) {
            let service = &self.services[*idx];
            // Waiting sockets share the same connection limit as running calls.
            let guard = ConnGuard::new(self.conns.get(), self.metrics.clone());
            let item = (con, cfg.clone(), guard);
            if service.svc.is_ready() && service.pending.borrow().is_empty() {
                service.dispatch(item)
            } else {
                service.pending.borrow_mut().push_back(item);
                Ok(())
            }
        } else {
            log::error!("Cannot get handler service for connection: {con:?}");
            Err(())
        }
    }
}
