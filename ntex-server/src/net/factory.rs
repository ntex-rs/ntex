use std::{marker::PhantomData, sync::Arc};

use ntex_io::Io;
use ntex_service::{IntoService, Pipeline, Service, State, cfg::SharedCfg};
use ntex_util::{future::BoxFuture, services::counter::CounterGuard};

use super::Token;

pub(super) type FactoryServiceType = Box<dyn FactoryService>;
type CreateResult = Vec<(Box<dyn NetService>, Arc<str>, Vec<(Token, SharedCfg)>)>;

pub(crate) trait NetService {
    fn call(&self, _: Io, _: CounterGuard);

    fn ready(&self) -> BoxFuture<'_, Result<(), ()>>;

    fn shutdown(&self) -> BoxFuture<'_, ()>;
}

pub(crate) trait FactoryService: Send {
    fn clo(&self) -> FactoryServiceType;

    fn create(&self) -> BoxFuture<'static, Result<CreateResult, &'static str>>;
}

struct Factory {
    name: Arc<str>,
    tokens: Vec<(Token, SharedCfg)>,
    wrapper: Box<dyn FactoryWrapper + Send>,
}

pub(crate) fn create_factory_service<F, S, St, I>(
    name: String,
    tokens: Vec<(Token, SharedCfg)>,
    f: F,
) -> FactoryServiceType
where
    F: AsyncFn() -> I + Send + Clone + 'static,
    I: IntoService<S, St> + 'static,
    S: Service<St, Req = Io> + 'static,
    St: State<Io> + 'static,
{
    let name: Arc<str> = Arc::from(name);

    Box::from(Factory {
        tokens,
        name: name.clone(),
        wrapper: Box::new(FactoryWrapperImpl { f, s: PhantomData }),
    })
}

impl FactoryService for Factory {
    fn clo(&self) -> FactoryServiceType {
        Box::new(Factory {
            name: self.name.clone(),
            tokens: self.tokens.clone(),
            wrapper: self.wrapper.clone(),
        })
    }

    fn create(&self) -> BoxFuture<'static, Result<CreateResult, &'static str>> {
        let name = self.name.clone();
        let tokens = self.tokens.clone();
        let factory_fut = self.wrapper.create();

        Box::pin(async move { Ok(vec![(factory_fut.await, name, tokens)]) })
    }
}

trait FactoryWrapper: Send {
    fn clone(&self) -> Box<dyn FactoryWrapper>;
    fn create(&self) -> BoxFuture<'static, Box<dyn NetService>>;
}

struct FactoryWrapperImpl<F, S, St, I> {
    f: F,
    s: PhantomData<(S, St, I)>,
}

impl<F, S, St, I> FactoryWrapper for FactoryWrapperImpl<F, S, St, I>
where
    F: AsyncFn() -> I + Send + Clone + 'static,
    I: IntoService<S, St> + 'static,
    S: Service<St, Req = Io> + 'static,
    St: State<Io> + 'static,
{
    fn clone(&self) -> Box<dyn FactoryWrapper> {
        Box::new(Self {
            f: self.f.clone(),
            s: PhantomData,
        })
    }

    fn create(&self) -> BoxFuture<'static, Box<dyn NetService>> {
        let f = self.f.clone();

        Box::pin(async move {
            let svc = (f)().await.into_service();
            let pipeline = Pipeline::with(svc.map(|_| ()).map_err(|_| ()));
            let svc: Box<dyn NetService> = Box::new(ServerService { pipeline });
            svc
        })
    }
}

pub(crate) struct ServerService {
    pub(crate) pipeline: Pipeline<Io, (), ()>,
}

impl NetService for ServerService {
    fn call(&self, io: Io, guard: CounterGuard) {
        let fut = self.pipeline.call_static(io);
        ntex_rt::spawn(async move {
            let _ = fut.await;
            drop(guard);
        });
    }

    fn ready(&self) -> BoxFuture<'_, Result<(), ()>> {
        Box::pin(async { self.pipeline.ready().await })
    }

    fn shutdown(&self) -> BoxFuture<'_, ()> {
        Box::pin(async { self.pipeline.shutdown().await })
    }
}

// SAFETY: Send cannot be provided authomatically because of E and R params
// but R always get executed in one thread and never leave it
unsafe impl Send for Factory {}
unsafe impl<F, S, St, I> Send for FactoryWrapperImpl<F, S, St, I> {}
