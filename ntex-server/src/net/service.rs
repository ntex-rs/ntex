use std::{fmt, sync::Arc, task::Context};

use ntex_bytes::{Pool, PoolRef};
use ntex_net::Io;
use ntex_service::{boxed, Service, ServiceCtx, ServiceFactory};
use ntex_util::{future::join_all, services::Counter, HashMap};

use crate::ServerConfiguration;

use super::accept::{AcceptNotify, AcceptorCommand};
use super::factory::{FactoryServiceType, NetService, OnAccept, OnWorkerStart};
use super::{socket::Connection, Token, MAX_CONNS_COUNTER};

pub(super) type BoxService = boxed::BoxService<Io, (), ()>;

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
            on_accept,
            on_worker_start,
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

impl ServiceFactory<Connection> for StreamService {
    type Response = ();
    type Error = ();
    type Service = StreamServiceImpl;
    type InitError = ();

    async fn create(&self, _: ()) -> Result<Self::Service, Self::InitError> {
        let mut tokens = HashMap::default();
        let mut services = Vec::new();

        for info in &self.services {
            match info.factory.create(()).await {
                Ok(svc) => {
                    log::trace!("Constructed server service for {:?}", info.tokens);
                    services.push(svc);
                    let idx = services.len() - 1;
                    for (token, tag) in &info.tokens {
                        tokens.insert(
                            *token,
                            (
                                idx,
                                *tag,
                                info.name.clone(),
                                info.pool.pool(),
                                info.pool.pool_ref(),
                            ),
                        );
                    }
                }
                Err(_) => {
                    log::error!("Cannot construct service: {:?}", info.tokens);
                    return Err(());
                }
            }
        }

        Ok(StreamServiceImpl {
            tokens,
            services,
            conns: MAX_CONNS_COUNTER.with(|conns| conns.clone()),
            on_accept: self.on_accept.as_ref().map(|f| f.clone_fn()),
        })
    }
}

pub struct StreamServiceImpl {
    tokens: HashMap<Token, (usize, &'static str, Arc<str>, Pool, PoolRef)>,
    services: Vec<BoxService>,
    conns: Counter,
    on_accept: Option<Box<dyn OnAccept + Send>>,
}

impl fmt::Debug for StreamServiceImpl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamServiceImpl")
            .field("tokens", &self.tokens)
            .field("services", &self.services)
            .field("conns", &self.conns)
            .finish()
    }
}

impl Service<Connection> for StreamServiceImpl {
    type Response = ();
    type Error = ();

    async fn ready(&self, ctx: ServiceCtx<'_, Self>) -> Result<(), Self::Error> {
        if !self.conns.is_available() {
            self.conns.available().await;
        }
        for (idx, svc) in self.services.iter().enumerate() {
            if ctx.ready(svc).await.is_err() {
                for (idx_, tag, _, _, _) in self.tokens.values() {
                    if idx == *idx_ {
                        log::error!("{}: Service readiness has failed", tag);
                        break;
                    }
                }
                return Err(());
            }
        }

        Ok(())
    }

    #[inline]
    fn poll(&self, cx: &mut Context<'_>) -> Result<(), Self::Error> {
        for svc in &self.services {
            svc.poll(cx)?;
        }
        Ok(())
    }

    async fn shutdown(&self) {
        let _ = join_all(self.services.iter().map(|svc| svc.shutdown())).await;
        log::info!(
            "Worker service shutdown, {} connections",
            super::num_connections()
        );
    }

    async fn call(&self, con: Connection, ctx: ServiceCtx<'_, Self>) -> Result<(), ()> {
        if let Some((idx, tag, name, _, pool)) = self.tokens.get(&con.token) {
            let mut io = con.io;
            if let Some(ref f) = self.on_accept {
                match f.run(name.clone(), io).await {
                    Ok(st) => io = st,
                    Err(_) => return Err(()),
                }
            }

            let stream: Io<_> = io.try_into().map_err(|e| {
                log::error!("Cannot convert to an async io stream: {}", e);
            })?;

            stream.set_tag(tag);
            stream.set_memory_pool(*pool);
            let guard = self.conns.get();
            let _ = ctx.call(&self.services[*idx], stream).await;
            drop(guard);
            Ok(())
        } else {
            log::error!("Cannot get handler service for connection: {:?}", con);
            Err(())
        }
    }
}
