use std::{task::Context, task::Poll};

use ntex_server::{ServerConfiguration, WorkerMessage};

use crate::io::Io;
use crate::service::{boxed, Service, ServiceCtx, ServiceFactory};
use crate::util::{HashMap, Pool, PoolRef};

use super::accept::{AcceptNotify, AcceptorCommand};
use super::counter::Counter;
use super::factory::{FactoryServiceType, NetService, OnWorkerStart};
use super::{socket::Connection, Token, MAX_CONNS_COUNTER};

pub type ServerMessage = WorkerMessage<Connection>;

pub(super) type BoxService = boxed::BoxService<Io, (), ()>;

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

pub struct StreamService {
    services: Vec<NetService>,
}

impl ServiceFactory<ServerMessage> for StreamService {
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

        let conns = MAX_CONNS_COUNTER.with(|conns| conns.priv_clone());

        Ok(StreamServiceImpl {
            tokens,
            services,
            conns,
        })
    }
}

pub struct StreamServiceImpl {
    tokens: HashMap<Token, (usize, &'static str, Pool, PoolRef)>,
    services: Vec<BoxService>,
    conns: Counter,
}

impl Service<ServerMessage> for StreamServiceImpl {
    type Response = ();
    type Error = ();

    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let mut ready = self.conns.available(cx);
        for (idx, tag, pool, _) in self.tokens.values() {
            match self.services[*idx].poll_ready(cx) {
                Poll::Pending => ready = false,
                Poll::Ready(Ok(())) => (),
                Poll::Ready(Err(_)) => {
                    log::error!("{}: Service readiness has failed", tag);
                    return Poll::Ready(Err(()));
                }
            }
            ready = pool.poll_ready(cx).is_ready() && ready;
        }
        if ready {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }

    fn poll_shutdown(&self, cx: &mut Context<'_>) -> Poll<()> {
        let mut ready = true;
        for svc in &self.services {
            match svc.poll_shutdown(cx) {
                Poll::Pending => ready = false,
                Poll::Ready(_) => (),
            }
        }
        if ready {
            log::info!(
                "Worker service shutdown, {} connections",
                super::num_connections()
            );
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }

    async fn call(&self, req: ServerMessage, ctx: ServiceCtx<'_, Self>) -> Result<(), ()> {
        match req {
            ServerMessage::New(con) => {
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
            _ => Ok(()),
        }
    }
}
