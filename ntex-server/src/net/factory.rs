use std::sync::Arc;

use ntex_io::Io;
use ntex_service::{IntoService, Pipeline, Service, State, cfg::SharedCfg};
use ntex_util::{future::BoxFuture, services::counter::CounterGuard};

use super::Token;

pub(super) type FactoryServiceType<St> = Box<dyn FactoryService<St>>;
type CreateResult = Vec<(Box<dyn NetService>, Arc<str>, Vec<(Token, SharedCfg)>)>;
type SvcFactory<St> = dyn Fn(St) -> BoxFuture<'static, Box<dyn NetService>>;

pub(crate) trait NetService {
    fn call(&self, _: Io, _: CounterGuard);

    fn ready(&self) -> BoxFuture<'_, Result<(), ()>>;

    fn shutdown(&self) -> BoxFuture<'_, ()>;
}

pub(crate) trait FactoryService<St>: Send {
    fn clo(&self) -> FactoryServiceType<St>;

    fn create(&self, st: St) -> BoxFuture<'static, Result<CreateResult, &'static str>>;
}

struct Factory<St> {
    name: Arc<str>,
    tokens: Vec<(Token, SharedCfg)>,
    fac: Arc<SvcFactory<St>>,
}

pub(crate) fn create_factory_service<F, S, St, I>(
    name: String,
    tokens: Vec<(Token, SharedCfg)>,
    f: F,
) -> FactoryServiceType<St>
where
    F: AsyncFn(&St) -> I + Send + Clone + 'static,
    I: IntoService<S, St> + 'static,
    S: Service<St, Req = Io> + 'static,
    St: State<Io> + 'static,
{
    Box::from(Factory {
        tokens,
        name: Arc::from(name),
        fac: Arc::new(move |st: St| {
            let f = f.clone();
            Box::pin(async move {
                let svc = (f)(&st).await.into_service();
                let pipeline = Pipeline::with_st(st, svc.map(|_| ()).map_err(|_| ()));
                let svc: Box<dyn NetService> = Box::new(ServerService { pipeline });
                svc
            })
        }),
    })
}

impl<St: 'static> FactoryService<St> for Factory<St> {
    fn clo(&self) -> FactoryServiceType<St> {
        Box::new(Factory {
            name: self.name.clone(),
            tokens: self.tokens.clone(),
            fac: self.fac.clone(),
        })
    }

    fn create(&self, st: St) -> BoxFuture<'static, Result<CreateResult, &'static str>> {
        let name = self.name.clone();
        let tokens = self.tokens.clone();
        let factory_fut = (self.fac)(st);

        Box::pin(async move { Ok(vec![(factory_fut.await, name, tokens)]) })
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
unsafe impl<St> Send for Factory<St> {}
