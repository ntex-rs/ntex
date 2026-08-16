use std::{fmt, sync::Arc};

use ntex_service::{Ctx, ReadyCtx, Service, cfg::SharedCfg};
use ntex_util::{HashMap, future::join_all, services::Counter};

use crate::ServerConfiguration;

use super::accept::{AcceptNotify, AcceptorCommand};
use super::factory::{FactoryServiceType, NetService};
use super::onaccept::{OnAccept, OnWorkerStart};
use super::{MAX_CONNS_COUNTER, Token, socket::Connection};

/// Net streaming server
pub struct StreamServer {
    notify: AcceptNotify,
    services: Vec<FactoryServiceType>,
    on_worker_start: Vec<Box<dyn OnWorkerStart + Send>>,
    on_accept: Option<Box<dyn OnAccept + Send>>,
}

impl StreamServer {
    pub(crate) fn new(
        notify: AcceptNotify,
        services: Vec<FactoryServiceType>,
        on_worker_start: Vec<Box<dyn OnWorkerStart + Send>>,
        on_accept: Option<Box<dyn OnAccept + Send>>,
    ) -> Self {
        Self {
            notify,
            services,
            on_worker_start,
            on_accept,
        }
    }
}

impl fmt::Debug for StreamServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamServer")
            .field("services", &self.services.len())
            .finish()
    }
}

/// Worker service factory.
impl ServerConfiguration for StreamServer {
    type Item = Connection;
    type Service = StreamService;

    /// Create service for handling connections
    async fn create(&self) -> Result<Self::Service, &'static str> {
        // on worker start callbacks
        for cb in &self.on_worker_start {
            cb.run().await?;
        }

        // construct services
        let mut tokens = HashMap::default();
        let mut services = Vec::new();

        for info in &self.services {
            for (svc, name, svc_tokens) in info.create().await? {
                services.push(svc);
                let idx = services.len() - 1;
                for (token, cfg) in &svc_tokens {
                    tokens.insert(*token, (idx, name.clone(), cfg.clone()));
                }
            }
        }

        Ok(StreamService {
            services,
            tokens,
            conns: MAX_CONNS_COUNTER.with(Clone::clone),
            on_accept: self.on_accept.as_ref().map(|f| f.clone_fn()),
        })
    }

    /// Pause the server.
    fn pause(&self) {
        self.notify.send(AcceptorCommand::Pause);
    }

    /// Resume the server.
    fn resume(&self) {
        self.notify.send(AcceptorCommand::Resume);
    }

    /// Terminate the server.
    fn terminate(&self) {
        self.notify.send(AcceptorCommand::Terminate);
    }

    /// Stop the server.
    async fn stop(&self) {
        let (tx, rx) = oneshot::channel();
        self.notify.send(AcceptorCommand::Stop(tx));
        let _ = rx.await;
    }
}

impl Clone for StreamServer {
    fn clone(&self) -> Self {
        Self {
            notify: self.notify.clone(),
            services: self.services.iter().map(|s| s.clo()).collect(),
            on_accept: self.on_accept.as_ref().map(|f| f.clone_fn()),
            on_worker_start: self.on_worker_start.iter().map(|f| f.clone_fn()).collect(),
        }
    }
}

pub struct StreamService {
    tokens: HashMap<Token, (usize, Arc<str>, SharedCfg)>,
    services: Vec<Box<dyn NetService>>,
    conns: Counter,
    on_accept: Option<Box<dyn OnAccept + Send>>,
}

impl fmt::Debug for StreamService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamService")
            .field("tokens", &self.tokens)
            .field("conns", &self.conns)
            .finish()
    }
}

impl Service for StreamService {
    type St = ();
    type Req = Connection;
    type Res = ();
    type Error = ();

    async fn ready(&self, _: ReadyCtx<'_, Self>) -> Result<(), Self::Error> {
        if !self.conns.is_available() {
            self.conns.available().await;
        }
        for (idx, svc) in self.services.iter().enumerate() {
            if svc.ready().await.is_err() {
                for (idx_, _, cfg) in self.tokens.values() {
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

    async fn call(&self, con: Connection, _: Ctx<'_, Self>) -> Result<(), ()> {
        if let Some((idx, name, cfg)) = self.tokens.get(&con.token) {
            let mut io = con.io;
            if let Some(ref f) = self.on_accept {
                match f.run(name.clone(), io).await {
                    Ok(st) => io = st,
                    Err(()) => return Err(()),
                }
            }

            let stream = io.convert(cfg.clone()).map_err(|e| {
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
