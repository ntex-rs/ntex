use std::{fmt, future::Future, marker::PhantomData, sync::Arc};

use ntex_io::Io;
use ntex_service::{Service, ServiceCtx, ServiceFactory, boxed, cfg::SharedCfg};
use ntex_util::future::BoxFuture;

use super::{Config, Token, socket::Stream};

pub(super) type BoxServerService = boxed::BoxServiceFactory<SharedCfg, Io, (), (), ()>;
pub(super) type FactoryServiceType = Box<dyn FactoryService>;

#[derive(Debug)]
pub(crate) struct NetService {
    pub(crate) name: Arc<str>,
    pub(crate) tokens: Vec<(Token, SharedCfg)>,
    pub(crate) config: SharedCfg,
    pub(crate) factory: BoxServerService,
}

pub(crate) trait FactoryService: Send {
    fn name(&self, _: Token) -> &str {
        ""
    }

    fn clone_factory(&self) -> FactoryServiceType;

    fn set_config(&mut self, _: Token, _: SharedCfg) {}

    fn create(&self) -> BoxFuture<'static, Result<Vec<NetService>, ()>>;
}

struct Factory {
    name: Arc<str>,
    tokens: Vec<(Token, SharedCfg)>,
    factory: Box<dyn FactoryWrapper + Send>,
}

pub(crate) fn create_boxed_factory<S>(name: String, factory: S) -> BoxServerService
where
    S: ServiceFactory<Io, SharedCfg> + 'static,
{
    boxed::factory(ServerServiceFactory {
        name: Arc::from(name),
        factory,
    })
}

pub(crate) fn create_factory_service<F, R>(
    name: String,
    tokens: Vec<(Token, SharedCfg)>,
    factory: F,
) -> FactoryServiceType
where
    F: AsyncFn(Config) -> R + Send + Clone + 'static,
    R: ServiceFactory<Io, SharedCfg> + 'static,
{
    let name: Arc<str> = Arc::from(name.clone());

    Box::from(Factory {
        tokens,
        name: name.clone(),
        factory: Box::new(FactoryWrapperImpl(async move |cfg| {
            boxed::factory(ServerServiceFactory {
                name: name.clone(),
                factory: (factory)(cfg).await,
            })
        })),
    })
}

struct FactoryWrapperImpl<F>(F);

trait FactoryWrapper: Send {
    fn clone(&self) -> Box<dyn FactoryWrapper>;
    fn run(&self, cfg: Config) -> BoxFuture<'static, BoxServerService>;
}

impl<F> FactoryWrapper for FactoryWrapperImpl<F>
where
    F: AsyncFn(Config) -> BoxServerService + Send + Clone + 'static,
{
    fn clone(&self) -> Box<dyn FactoryWrapper> {
        Box::new(Self(self.0.clone()))
    }

    fn run(&self, cfg: Config) -> BoxFuture<'static, BoxServerService> {
        let f = self.0.clone();
        Box::pin(async move { f(cfg).await })
    }
}

impl FactoryService for Factory {
    fn name(&self, _: Token) -> &str {
        &self.name
    }

    fn clone_factory(&self) -> FactoryServiceType {
        Box::new(Factory {
            name: self.name.clone(),
            tokens: self.tokens.clone(),
            factory: self.factory.clone(),
        })
    }

    fn set_config(&mut self, token: Token, cfg: SharedCfg) {
        for item in &mut self.tokens {
            if item.0 == token {
                item.1 = cfg;
            }
        }
    }

    fn create(&self) -> BoxFuture<'static, Result<Vec<NetService>, ()>> {
        let cfg = Config::default();
        let name = self.name.clone();
        let mut tokens = self.tokens.clone();
        let factory_fut = self.factory.run(cfg.clone());

        Box::pin(async move {
            //let factory = factory_fut.await.map_err(|_| {
            //log::error!("Cannot create {name:?} service");
            //})?;
            let factory = factory_fut.await;
            if let Some(config) = cfg.get_config() {
                for item in &mut tokens {
                    item.1 = config;
                }
            }

            Ok(vec![NetService {
                tokens,
                factory,
                name: Arc::from(name),
                config: cfg.get_config().unwrap_or_default(),
            }])
        })
    }
}

struct ServerServiceFactory<S> {
    name: Arc<str>,
    factory: S,
}

impl<S> ServiceFactory<Io, SharedCfg> for ServerServiceFactory<S>
where
    S: ServiceFactory<Io, SharedCfg>,
{
    type Response = ();
    type Error = ();
    type Service = ServerService<S::Service>;
    type InitError = ();

    async fn create(&self, cfg: SharedCfg) -> Result<Self::Service, Self::InitError> {
        self.factory
            .create(cfg)
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

    async fn ready(&self, ctx: ServiceCtx<'_, Self>) -> Result<(), ()> {
        ctx.ready(&self.inner).await.map_err(|_| ())
    }

    async fn call(&self, req: Io, ctx: ServiceCtx<'_, Self>) -> Result<(), ()> {
        ctx.call(&self.inner, req).await.map(|_| ()).map_err(|_| ())
    }

    ntex_service::forward_shutdown!(inner);
}

// SAFETY: Send cannot be provided authomatically because of E and R params
// but R always get executed in one thread and never leave it
unsafe impl Send for Factory {}

pub(crate) trait OnWorkerStart {
    fn clone_fn(&self) -> Box<dyn OnWorkerStart + Send>;

    fn run(&self) -> BoxFuture<'static, Result<(), ()>>;
}

pub(super) struct OnWorkerStartWrapper<F, E> {
    pub(super) f: F,
    pub(super) _t: PhantomData<E>,
}

unsafe impl<F, E> Send for OnWorkerStartWrapper<F, E> where F: Send {}

impl<F, E> OnWorkerStartWrapper<F, E>
where
    F: AsyncFn() -> Result<(), E> + Send + Clone + 'static,
    E: fmt::Display + 'static,
{
    pub(super) fn create(f: F) -> Box<dyn OnWorkerStart + Send> {
        Box::new(Self { f, _t: PhantomData })
    }
}

impl<F, E> OnWorkerStart for OnWorkerStartWrapper<F, E>
where
    F: AsyncFn() -> Result<(), E> + Send + Clone + 'static,
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
                log::error!("On worker start callback failed: {e}");
            })
        })
    }
}

pub(crate) trait OnAccept {
    fn clone_fn(&self) -> Box<dyn OnAccept + Send>;

    fn run(&self, name: Arc<str>, stream: Stream)
    -> BoxFuture<'static, Result<Stream, ()>>;
}

pub(super) struct OnAcceptWrapper<F, R, E> {
    pub(super) f: F,
    pub(super) _t: PhantomData<(R, E)>,
}

unsafe impl<F, R, E> Send for OnAcceptWrapper<F, R, E> where F: Send {}

impl<F, R, E> OnAcceptWrapper<F, R, E>
where
    F: Fn(Arc<str>, Stream) -> R + Send + Clone + 'static,
    R: Future<Output = Result<Stream, E>> + 'static,
    E: fmt::Display + 'static,
{
    pub(super) fn create(f: F) -> Box<dyn OnAccept + Send> {
        Box::new(Self { f, _t: PhantomData })
    }
}

impl<F, R, E> OnAccept for OnAcceptWrapper<F, R, E>
where
    F: Fn(Arc<str>, Stream) -> R + Send + Clone + 'static,
    R: Future<Output = Result<Stream, E>> + 'static,
    E: fmt::Display + 'static,
{
    fn clone_fn(&self) -> Box<dyn OnAccept + Send> {
        Box::new(Self {
            f: self.f.clone(),
            _t: PhantomData,
        })
    }

    fn run(
        &self,
        name: Arc<str>,
        stream: Stream,
    ) -> BoxFuture<'static, Result<Stream, ()>> {
        let f = self.f.clone();
        Box::pin(async move {
            (f)(name, stream).await.map_err(|e| {
                log::error!("On accept callback failed: {e}");
            })
        })
    }
}
