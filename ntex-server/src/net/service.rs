use std::{fmt, sync::Arc, task::Context};

use ntex_io::Io;
use ntex_service::{
    PipelineSvc, Service, ServiceCtx, ServiceFactory, boxed, cfg::SharedCfg,
};
use ntex_util::{HashMap, future::join_all, services::Counter};

use crate::ServerConfiguration;

use super::accept::{AcceptNotify, AcceptorCommand};
use super::factory::{FactoryServiceType, NetService, OnAccept, OnWorkerStart};
use super::{MAX_CONNS_COUNTER, Token, socket::Connection};

pub(super) type BoxService = PipelineSvc<boxed::BoxService<Io, (), ()>, ()>;

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
    type Factory = StreamService;

    /// Create service factory for handling `WorkerMessage<T>` messages.
    async fn create(&self) -> Result<Self::Factory, ()> {
        // on worker start callbacks
        for cb in &self.on_worker_start {
            cb.run().await?;
        }

        // construct services
        let mut services = Vec::new();
        for svc in &self.services {
            services.extend(svc.create().await?);
        }

        Ok(StreamService {
            services,
            on_accept: self.on_accept.as_ref().map(|f| f.clone_fn()),
        })
    }

    /// Server is paused
    fn paused(&self) {
        self.notify.send(AcceptorCommand::Pause);
    }

    /// Server is resumed
    fn resumed(&self) {
        self.notify.send(AcceptorCommand::Resume);
    }

    /// Server is stopped
    fn terminate(&self) {
        self.notify.send(AcceptorCommand::Terminate);
    }

    /// Server is stopped
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
            services: self.services.iter().map(|s| s.clone_factory()).collect(),
            on_accept: self.on_accept.as_ref().map(|f| f.clone_fn()),
            on_worker_start: self.on_worker_start.iter().map(|f| f.clone_fn()).collect(),
        }
    }
}

pub struct StreamService {
    services: Vec<NetService>,
    on_accept: Option<Box<dyn OnAccept + Send>>,
}

impl fmt::Debug for StreamService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamService")
            .field("services", &self.services)
            .finish()
    }
}

impl ServiceFactory<Connection, ()> for StreamService {
    type Response = ();
    type Error = ();
    type Service = StreamServiceImpl;
    type InitError = ();
    type Data = ();

    async fn create(&self, _: ()) -> Result<Self::Service, Self::InitError> {
        Ok(StreamServiceImpl {
            on_accept: self.on_accept.as_ref().map(|f| f.clone_fn()),
        })
    }

    async fn map_data(
        &self,
        _: &(),
        _: &Self::Data,
    ) -> Result<StreamServiceData, Self::InitError> {
        let mut tokens = HashMap::default();
        let mut services = Vec::new();

        for info in &self.services {
            if let Ok(svc) = info.factory.pipeline(info.config.clone(), &()).await {
                log::trace!("Constructed server service for {:?}", info.tokens);
                services.push(PipelineSvc::new(svc));
                let idx = services.len() - 1;
                for (token, cfg) in &info.tokens {
                    tokens.insert(*token, (idx, info.name.clone(), cfg.clone()));
                }
            } else {
                log::error!("Cannot construct service: {:?}", info.tokens);
                return Err(());
            }
        }

        Ok(StreamServiceData {
            tokens,
            services,
            conns: MAX_CONNS_COUNTER.with(Clone::clone),
        })
    }
}

pub struct StreamServiceImpl {
    on_accept: Option<Box<dyn OnAccept + Send>>,
}

#[derive(Debug)]
pub struct StreamServiceData {
    tokens: HashMap<Token, (usize, Arc<str>, SharedCfg)>,
    services: Vec<BoxService>,
    conns: Counter,
}

impl fmt::Debug for StreamServiceImpl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamServiceImpl").finish()
    }
}

impl Service<Connection> for StreamServiceImpl {
    type Response = ();
    type Error = ();
    type Data = StreamServiceData;

    async fn ready(
        &self,
        data: &Self::Data,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<(), Self::Error> {
        if !data.conns.is_available() {
            data.conns.available().await;
        }
        for (idx, svc) in data.services.iter().enumerate() {
            if ctx.ready(svc, &()).await.is_err() {
                for (idx_, _, cfg) in data.tokens.values() {
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

    #[inline]
    fn poll(&self, data: &Self::Data, cx: &mut Context<'_>) -> Result<(), Self::Error> {
        for svc in &data.services {
            svc.poll(&(), cx)?;
        }
        Ok(())
    }

    async fn shutdown(&self, data: &Self::Data) {
        let _ = join_all(data.services.iter().map(|svc| svc.shutdown(&()))).await;
        log::info!(
            "Worker service shutdown, {} connections",
            super::num_connections()
        );
    }

    async fn call(
        &self,
        con: Connection,
        data: &Self::Data,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<(), ()> {
        if let Some((idx, name, cfg)) = data.tokens.get(&con.token) {
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

            let guard = data.conns.get();
            let _ = ctx.call(&data.services[*idx], stream, &()).await;
            drop(guard);
            Ok(())
        } else {
            log::error!("Cannot get handler service for connection: {con:?}");
            Err(())
        }
    }
}
