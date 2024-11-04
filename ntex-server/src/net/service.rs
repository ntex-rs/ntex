use std::{fmt, future::poll_fn, future::Future, pin::Pin, task::Poll};

use ntex_bytes::{Pool, PoolRef};
use ntex_net::Io;
use ntex_service::{boxed, Service, ServiceCtx, ServiceFactory};
use ntex_util::{future::join_all, services::Counter, HashMap};

use crate::ServerConfiguration;

use super::accept::{AcceptNotify, AcceptorCommand};
use super::factory::{FactoryServiceType, NetService, OnWorkerStart};
use super::{socket::Connection, Token, MAX_CONNS_COUNTER};

pub(super) type BoxService = boxed::BoxService<Io, (), ()>;

/// Net streaming server
pub struct StreamServer {
    notify: AcceptNotify,
    services: Vec<FactoryServiceType>,
    on_worker_start: Vec<Box<dyn OnWorkerStart + Send>>,
}

impl StreamServer {
    pub(crate) fn new(
        notify: AcceptNotify,
        services: Vec<FactoryServiceType>,
        on_worker_start: Vec<Box<dyn OnWorkerStart + Send>>,
    ) -> Self {
        Self {
            notify,
            services,
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

        Ok(StreamService { services })
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
            on_worker_start: self.on_worker_start.iter().map(|f| f.clone_fn()).collect(),
        }
    }
}

#[derive(Debug)]
pub struct StreamService {
    services: Vec<NetService>,
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
                    log::debug!("Constructed server service for {:?}", info.tokens);
                    services.push(svc);
                    let idx = services.len() - 1;
                    for (token, tag) in &info.tokens {
                        tokens.insert(
                            *token,
                            (idx, *tag, info.pool.pool(), info.pool.pool_ref()),
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
        })
    }
}

#[derive(Debug)]
pub struct StreamServiceImpl {
    tokens: HashMap<Token, (usize, &'static str, Pool, PoolRef)>,
    services: Vec<BoxService>,
    conns: Counter,
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
                for (idx_, tag, _, _) in self.tokens.values() {
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
    async fn not_ready(&self) {
        if self.conns.is_available() {
            let mut futs: Vec<_> = self
                .services
                .iter()
                .map(|s| Box::pin(s.not_ready()))
                .collect();

            ntex_util::future::select(
                self.conns.unavailable(),
                poll_fn(move |cx| {
                    for f in &mut futs {
                        if Pin::new(f).poll(cx).is_ready() {
                            return Poll::Ready(());
                        }
                    }
                    Poll::Pending
                }),
            )
            .await;
        }
    }

    async fn shutdown(&self) {
        let _ = join_all(self.services.iter().map(|svc| svc.shutdown())).await;
        log::info!(
            "Worker service shutdown, {} connections",
            super::num_connections()
        );
    }

    async fn call(&self, con: Connection, ctx: ServiceCtx<'_, Self>) -> Result<(), ()> {
        if let Some((idx, tag, _, pool)) = self.tokens.get(&con.token) {
            let stream: Io<_> = con.io.try_into().map_err(|e| {
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
