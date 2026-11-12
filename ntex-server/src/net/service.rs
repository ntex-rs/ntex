use std::{fmt, io, pin::Pin, sync::Arc};

use ntex_io::{IoConfig, IoMetricsGroup};
use ntex_service::{BindCtx, Ctx, Service, cfg::SharedCfg};
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
                services.push(svc);
                let idx = services.len() - 1;
                for (token, cfg) in &svc_tokens {
                    // listener connections have their own metrics
                    group.register(&cfg.get::<IoConfig>());
                    tokens.insert(*token, (idx, cfg.clone()));
                }
            }
        }

        Ok(StreamService {
            services,
            tokens,
            conns: Counter::new(self.max_conns),
            metrics: WorkerSlot::current(),
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
    services: Vec<Box<dyn NetService>>,
    conns: Counter,
    metrics: Option<Arc<WorkerSlot>>,
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
        for service in &self.services {
            service.bind(&ctx);
        }
    }

    async fn ready(&self, _: Ctx<'_, Self, ()>) -> Result<(), Self::Error> {
        if !self.conns.is_available() {
            self.conns.available().await;
        }
        for (idx, svc) in self.services.iter().enumerate() {
            if svc.ready().await.is_err() {
                for (idx_, cfg) in self.tokens.values() {
                    if idx == *idx_ {
                        log::error!("{}: Service readiness has failed", cfg.tag());
                        break;
                    }
                }
                return Err(());
            }
        }

        Ok(())
    }

    async fn shutdown(&self, _: Ctx<'_, Self, ()>) {
        let _ = join_all(self.services.iter().map(|s| s.shutdown())).await;
        log::info!(
            "Worker service shutdown, {} connections",
            self.conns.total()
        );
    }

    #[allow(clippy::unused_async_trait_impl)]
    async fn call(&self, con: Connection, _: Ctx<'_, Self, ()>) -> Result<(), ()> {
        if let Some((idx, cfg)) = self.tokens.get(&con.token) {
            let stream = con.io.convert(cfg.clone()).map_err(|e| {
                log::error!("Cannot convert to an async io stream: {e}");
            })?;

            let guard = ConnGuard::new(self.conns.get(), self.metrics.clone());
            self.services[*idx].call(stream, guard);
            Ok(())
        } else {
            log::error!("Cannot get handler service for connection: {con:?}");
            Err(())
        }
    }
}
