use std::fmt;

use ntex_service::{Ctx, ReadyCtx, Service, cfg::SharedCfg};
use ntex_util::{HashMap, future::join_all, services::Counter};

use crate::ServerConfiguration;

use super::accept::{AcceptNotify, AcceptorCommand};
use super::factory::{FactoryServiceType, NetService};
use super::state::StateFactory;
use super::{MAX_CONNS_COUNTER, Token, socket::Connection};

/// Net streaming server
pub struct StreamServer<St> {
    accept: AcceptNotify,
    state: Box<dyn StateFactory<St> + Send>,
    services: Vec<FactoryServiceType<St>>,
}

impl<St: Clone> StreamServer<St> {
    pub(crate) fn new(
        accept: AcceptNotify,
        state: Box<dyn StateFactory<St> + Send>,
        services: Vec<FactoryServiceType<St>>,
    ) -> Self {
        Self {
            accept,
            state,
            services,
        }
    }
}

impl<St> fmt::Debug for StreamServer<St> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamServer")
            .field("services", &self.services.len())
            .finish()
    }
}

/// Worker service factory.
impl<St: Clone + 'static> ServerConfiguration for StreamServer<St> {
    type Item = Connection;
    type Service = StreamService;

    /// Create service for handling connections
    async fn create(&self) -> Result<Self::Service, &'static str> {
        // construct state
        let state = self.state.create().await?;

        // construct services
        let mut tokens = HashMap::default();
        let mut services = Vec::new();

        for info in &self.services {
            for (svc, _, svc_tokens) in info.create(state.clone()).await? {
                services.push(svc);
                let idx = services.len() - 1;
                for (token, cfg) in &svc_tokens {
                    tokens.insert(*token, (idx, cfg.clone()));
                }
            }
        }

        Ok(StreamService {
            services,
            tokens,
            conns: MAX_CONNS_COUNTER.with(Clone::clone),
        })
    }

    /// Pause the server.
    fn pause(&self) {
        self.accept.send(AcceptorCommand::Pause);
    }

    /// Resume the server.
    fn resume(&self) {
        self.accept.send(AcceptorCommand::Resume);
    }

    /// Terminate the server.
    fn terminate(&self) {
        self.accept.send(AcceptorCommand::Terminate);
    }

    /// Stop the server.
    async fn stop(&self) {
        let (tx, rx) = oneshot::channel();
        self.accept.send(AcceptorCommand::Stop(tx));
        let _ = rx.await;
    }
}

impl<St> Clone for StreamServer<St> {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clo(),
            accept: self.accept.clone(),
            services: self.services.iter().map(|s| s.clo()).collect(),
        }
    }
}

pub struct StreamService {
    tokens: HashMap<Token, (usize, SharedCfg)>,
    services: Vec<Box<dyn NetService>>,
    conns: Counter,
}

impl fmt::Debug for StreamService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamService")
            .field("tokens", &self.tokens)
            .field("conns", &self.conns)
            .finish()
    }
}

impl Service<()> for StreamService {
    type Req = Connection;
    type Res = ();
    type Error = ();

    async fn ready(&self, _: ReadyCtx<'_, Self, ()>) -> Result<(), Self::Error> {
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

    async fn shutdown(&self) {
        let _ = join_all(self.services.iter().map(|s| s.shutdown())).await;
        log::info!(
            "Worker service shutdown, {} connections",
            super::num_connections()
        );
    }

    #[allow(clippy::unused_async_trait_impl)]
    async fn call(&self, con: Connection, _: Ctx<'_, Self, ()>) -> Result<(), ()> {
        if let Some((idx, cfg)) = self.tokens.get(&con.token) {
            let stream = con.io.convert(cfg.clone()).map_err(|e| {
                log::error!("Cannot convert to an async io stream: {e}");
            })?;

            self.services[*idx].call(stream, self.conns.get());
            Ok(())
        } else {
            log::error!("Cannot get handler service for connection: {con:?}");
            Err(())
        }
    }
}
