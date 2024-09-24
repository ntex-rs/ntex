use std::{fmt, future::Future, marker::PhantomData};

use ntex_bytes::PoolId;
use ntex_net::Io;
use ntex_service::{boxed, Service, ServiceCtx, ServiceFactory};
use ntex_util::future::{BoxFuture, Ready};

use super::{Config, Token};

pub(super) type BoxServerService = boxed::BoxServiceFactory<(), Io, (), (), ()>;
pub(crate) type FactoryServiceType = Box<dyn FactoryService>;

#[derive(Debug)]
pub(crate) struct NetService {
    pub(crate) tokens: Vec<(Token, &'static str)>,
    pub(crate) factory: BoxServerService,
    pub(crate) pool: PoolId,
}

pub(crate) trait FactoryService: Send {
    fn name(&self, _: Token) -> &str {
        ""
    }

    fn set_tag(&mut self, _: Token, _: &'static str) {}

    fn clone_factory(&self) -> Box<dyn FactoryService>;

    fn create(&self) -> BoxFuture<'static, Result<Vec<NetService>, ()>>;
}

pub(crate) fn create_boxed_factory<S>(name: String, factory: S) -> BoxServerService
where
    S: ServiceFactory<Io> + 'static,
{
    boxed::factory(ServerServiceFactory { name, factory })
}

pub(crate) fn create_factory_service<F, R>(
    name: String,
    tokens: Vec<(Token, &'static str)>,
    factory: F,
) -> Box<dyn FactoryService>
where
    F: Fn(Config) -> R + Send + Clone + 'static,
    R: ServiceFactory<Io> + 'static,
{
    Box::new(Factory {
        tokens,
        name: name.clone(),
        factory: move |cfg| {
            Ready::Ok::<_, &'static str>(create_boxed_factory(name.clone(), (factory)(cfg)))
        },
        _t: PhantomData,
    })
}

struct Factory<F, R, E> {
    name: String,
    tokens: Vec<(Token, &'static str)>,
    factory: F,
    _t: PhantomData<(R, E)>,
}

impl<F, R, E> FactoryService for Factory<F, R, E>
where
    F: Fn(Config) -> R + Send + Clone + 'static,
    R: Future<Output = Result<BoxServerService, E>> + 'static,
    E: fmt::Display + 'static,
{
    fn name(&self, _: Token) -> &str {
        &self.name
    }

    fn clone_factory(&self) -> Box<dyn FactoryService> {
        Box::new(Self {
            name: self.name.clone(),
            tokens: self.tokens.clone(),
            factory: self.factory.clone(),
            _t: PhantomData,
        })
    }

    fn set_tag(&mut self, token: Token, tag: &'static str) {
        for item in &mut self.tokens {
            if item.0 == token {
                item.1 = tag;
            }
        }
    }

    fn create(&self) -> BoxFuture<'static, Result<Vec<NetService>, ()>> {
        let cfg = Config::default();
        let name = self.name.clone();
        let mut tokens = self.tokens.clone();
        let factory_fut = (self.factory)(cfg.clone());

        Box::pin(async move {
            let factory = factory_fut.await.map_err(|_| {
                log::error!("Cannot create {:?} service", name);
            })?;
            if let Some(tag) = cfg.get_tag() {
                for item in &mut tokens {
                    item.1 = tag;
                }
            }

            Ok(vec![NetService {
                tokens,
                factory,
                pool: cfg.get_pool_id(),
            }])
        })
    }
}

struct ServerServiceFactory<S> {
    name: String,
    factory: S,
}

impl<S> ServiceFactory<Io> for ServerServiceFactory<S>
where
    S: ServiceFactory<Io>,
{
    type Response = ();
    type Error = ();
    type Service = ServerService<S::Service>;
    type InitError = ();

    async fn create(&self, _: ()) -> Result<Self::Service, Self::InitError> {
        self.factory
            .create(())
            .await
            .map(|inner| ServerService { inner })
            .map_err(|_| log::error!("Cannot construct {:?} service", self.name))
    }
}

struct ServerService<S> {
    inner: S,
}

impl<S> Service<Io> for ServerService<S>
where
    S: Service<Io>,
{
    type Response = ();
    type Error = ();

    ntex_service::forward_shutdown!(inner);

    async fn ready(&self, ctx: ServiceCtx<'_, Self>) -> Result<(), ()> {
        ctx.ready(&self.inner).await.map_err(|_| ())
    }

    async fn call(&self, req: Io, ctx: ServiceCtx<'_, Self>) -> Result<(), ()> {
        ctx.call(&self.inner, req).await.map(|_| ()).map_err(|_| ())
    }
}

// SAFETY: Send cannot be provided authomatically because of E and R params
// but R always get executed in one thread and never leave it
unsafe impl<F, R, E> Send for Factory<F, R, E> where F: Send {}

pub(crate) trait OnWorkerStart {
    fn clone_fn(&self) -> Box<dyn OnWorkerStart + Send>;

    fn run(&self) -> BoxFuture<'static, Result<(), ()>>;
}

pub(super) struct OnWorkerStartWrapper<F, R, E> {
    pub(super) f: F,
    pub(super) _t: PhantomData<(R, E)>,
}

unsafe impl<F, R, E> Send for OnWorkerStartWrapper<F, R, E> where F: Send {}

impl<F, R, E> OnWorkerStartWrapper<F, R, E>
where
    F: Fn() -> R + Send + Clone + 'static,
    R: Future<Output = Result<(), E>> + 'static,
    E: fmt::Display + 'static,
{
    pub(super) fn create(f: F) -> Box<dyn OnWorkerStart + Send> {
        Box::new(Self { f, _t: PhantomData })
    }
}

impl<F, R, E> OnWorkerStart for OnWorkerStartWrapper<F, R, E>
where
    F: Fn() -> R + Send + Clone + 'static,
    R: Future<Output = Result<(), E>> + 'static,
    E: fmt::Display + 'static,
{
    fn clone_fn(&self) -> Box<dyn OnWorkerStart + Send> {
        Box::new(Self {
            f: self.f.clone(),
            _t: PhantomData,
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
