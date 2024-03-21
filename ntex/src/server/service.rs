use std::{fmt, future::Future, marker, task::Context, task::Poll};

use ntex_server::{ServerConfiguration, WorkerMessage};

use crate::io::Io;
use crate::service::{boxed, Service, ServiceCtx, ServiceFactory};
use crate::util::{BoxFuture, HashMap, Pool, PoolId, PoolRef};

use super::{accept::AcceptNotify, socket::Connection, Config, Token};

pub type ServerMessage = WorkerMessage<Connection>;

pub(super) type BoxService = boxed::BoxService<Io, (), ()>;
pub(super) type BoxServiceFactory = boxed::BoxServiceFactory<(), Io, (), (), ()>;

pub struct StreamServer {
    notify: AcceptNotify,
    services: Vec<Factory>,
    on_worker_start: Vec<Box<dyn OnWorkerStart + Send>>,
}

/// Worker service factory.
impl ServerConfiguration for StreamServer {
    type Item = Connection;
    type Factory = StreamService;

    /// Create service factory for handling `WorkerMessage<T>` messages.
    async fn create(&self) -> Self::Factory {
        todo!()
    }

    /// Server is paused.
    fn paused(&self) {}

    /// Server is resumed.
    fn resumed(&self) {}

    /// Server is stopped
    async fn stop(&self) {}
}

impl Clone for StreamServer {
    fn clone(&self) -> Self {
        Self {
            notify: self.notify.clone(),
            services: self.services.clone(),
            on_worker_start: self.on_worker_start.iter().map(|f| *f.clone()).collect(),
        }
    }
}

struct NetService {
    tokens: Vec<(Token, &'static str)>,
    service: BoxServiceFactory,
}

pub struct StreamService {
    pool: PoolId,
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
            match info.service.create(()).await {
                Ok(svc) => {
                    services.push(svc);
                    let idx = services.len() - 1;
                    for (token, tag) in &info.tokens {
                        tokens.insert(*token, (idx, *tag));
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
            pool: self.pool.pool(),
            pool_ref: self.pool.pool_ref(),
        })
    }
}

pub(super) struct StreamServiceImpl {
    tokens: HashMap<Token, (usize, &'static str)>,
    services: Vec<BoxService>,
    pool: Pool,
    pool_ref: PoolRef,
}

impl StreamServiceImpl {
    pub(crate) fn new(pid: PoolId) -> Self {
        Self {
            pool: pid.pool(),
            pool_ref: pid.pool_ref(),
            tokens: HashMap::default(),
            services: Vec::new(),
        }
    }
}

impl Service<ServerMessage> for StreamServiceImpl {
    type Response = ();
    type Error = ();

    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let mut ready = true;
        for (idx, tag) in self.tokens.values() {
            match self.services[*idx].poll_ready(cx) {
                Poll::Pending => ready = false,
                Poll::Ready(Ok(())) => (),
                Poll::Ready(Err(_)) => {
                    log::error!("{}: Service readiness has failed", tag);
                    return Poll::Ready(Err(()));
                }
            }
        }
        let ready = self.pool.poll_ready(cx).is_ready() && ready;
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
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }

    async fn call(&self, req: ServerMessage, ctx: ServiceCtx<'_, Self>) -> Result<(), ()> {
        match req {
            ServerMessage::New(con) => {
                if let Some((idx, tag)) = self.tokens.get(&con.token) {
                    let stream: Io<_> = con.io.try_into().map_err(|e| {
                        log::error!("Cannot convert to an async io stream: {}", e);
                    })?;

                    stream.set_tag(tag);
                    stream.set_memory_pool(self.pool_ref);
                    let _ = ctx.call(&self.services[*idx], stream).await;
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

pub(super) struct Factory {
    name: String,
    tokens: Vec<(Token, &'static str)>,
    factory: Box<dyn FactoryService + Send>,
}

impl Factory {
    pub(crate) fn new<F, R>(
        name: String,
        tokens: Vec<(Token, &'static str)>,
        factory: F,
    ) -> Self
    where
        F: Fn(Config) -> R + Send + Clone + 'static,
        R: ServiceFactory<Io>,
    {
        Self {
            name,
            tokens,
            factory: FactoryServiceWrapper::create(factory),
        }
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn set_tag(&mut self, token: Token, tag: &'static str) {
        for item in &mut self.tokens {
            if item.0 == token {
                item.1 = tag;
            }
        }
    }
}

impl Clone for Factory {
    fn clone(&self) -> Self {
        Self {
            name: self.name,
            tokens: self.tokens.clone(),
            factory: self.factory.clone(),
        }
    }
}

// impl<F> InternalServiceFactory for Factory<F>
// where
//     F: StreamServiceFactory,
// {
//     fn create(&self) -> BoxFuture<'static, Result<Vec<(Token, BoxedServerService)>, ()>> {
//         let token = self.token;
//         let tag = self.tag;
//         let cfg = Config::default();
//         let pool = cfg.get_pool_id();
//         let factory = self.inner.create(cfg);

//         Box::pin(async move {
//             match factory.create(()).await {
//                 Ok(inner) => {
//                     let service = boxed::service(StreamService::new(inner, tag, pool));
//                     Ok(vec![(token, service)])
//                 }
//                 Err(_) => Err(()),
//             }
//         })
//     }
// }

pub(super) trait FactoryService {
    fn clone(&self) -> Box<dyn FactoryService + Send>;

    fn run(&self, config: Config) -> BoxServiceFactory;
}

pub(super) struct FactoryServiceWrapper<F, R> {
    pub(super) f: F,
    pub(super) _t: marker::PhantomData<R>,
}

impl<F, R> FactoryServiceWrapper<F, R>
where
    F: Fn(Config) -> R + Send + Clone + 'static,
    R: ServiceFactory<Io> + 'static,
{
    pub(super) fn create(f: F) -> Box<dyn FactoryService + Send> {
        Box::new(Self {
            f,
            _t: marker::PhantomData,
        })
    }
}

unsafe impl<F, R> Send for FactoryServiceWrapper<F, R> where F: Send {}

impl<F, R> FactoryService for FactoryServiceWrapper<F, R>
where
    F: Fn(Config) -> R + Send + Clone + 'static,
    R: ServiceFactory<Io> + 'static,
{
    fn clone(&self) -> Box<dyn FactoryService + Send> {
        Box::new(Self {
            f: self.f.clone(),
            _t: marker::PhantomData,
        })
    }

    fn run(&self, config: Config) -> BoxServiceFactory {
        // boxed::factory((self.f)(config))
        todo!()
    }
}

pub(super) trait OnWorkerStart {
    fn clone(&self) -> Box<dyn OnWorkerStart + Send>;

    fn run(&self) -> BoxFuture<'static, Result<(), ()>>;
}

pub(super) struct OnWorkerStartWrapper<F, R, E> {
    pub(super) f: F,
    pub(super) _t: marker::PhantomData<(R, E)>,
}

unsafe impl<F, R, E> Send for OnWorkerStartWrapper<F, R, E> where F: Send {}

impl<F, R, E> OnWorkerStartWrapper<F, R, E>
where
    F: Fn() -> R + Send + Clone + 'static,
    R: Future<Output = Result<(), E>> + 'static,
    E: fmt::Display + 'static,
{
    pub(super) fn create(f: F) -> Box<dyn OnWorkerStart + Send> {
        Box::new(Self {
            f,
            _t: marker::PhantomData,
        })
    }
}

impl<F, R, E> OnWorkerStart for OnWorkerStartWrapper<F, R, E>
where
    F: Fn() -> R + Send + Clone + 'static,
    R: Future<Output = Result<(), E>> + 'static,
    E: fmt::Display + 'static,
{
    fn clone(&self) -> Box<dyn OnWorkerStart + Send> {
        Box::new(Self {
            f: self.f.clone(),
            _t: marker::PhantomData,
        })
    }

    fn run(&self) -> BoxFuture<'static, Result<(), ()>> {
        let f = self.f.clone();
        Box::pin(async move {
            (f)().await.map_err(|e| {
                log::error!("On worker start callback failed: {}", e);
            })
        })
    }
}
