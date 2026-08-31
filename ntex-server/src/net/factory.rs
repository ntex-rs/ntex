use std::{io, sync::Arc};

use ntex_io::Io;
use ntex_service::{IntoService, Pipeline, Service, cfg::SharedCfg};
use ntex_util::{future::BoxFuture, services::counter::CounterGuard};

use super::{ServerAppConfig, Token};

pub(super) type FactoryServiceType<St> = Box<dyn FactoryService<St>>;
type CreateResult = Vec<(Box<dyn NetService>, Arc<str>, Vec<(Token, SharedCfg)>)>;
type SvcFactory<St> = dyn Fn(St) -> BoxFuture<'static, Box<dyn NetService>>;

pub(crate) trait NetService {
    fn call(&self, _: Io, _: CounterGuard);

    fn ready(&self) -> BoxFuture<'_, Result<(), ()>>;

    fn shutdown(&self) -> BoxFuture<'_, ()>;
}

pub(crate) trait FactoryService<Cfg: ServerAppConfig>: Send {
    fn clo(&self) -> FactoryServiceType<Cfg>;

    fn create(&self, st: Cfg::Config) -> BoxFuture<'static, io::Result<CreateResult>>;
}

struct Factory<Cfg: ServerAppConfig> {
    name: Arc<str>,
    tokens: Vec<(Token, SharedCfg)>,
    fac: Arc<SvcFactory<Cfg::Config>>,
}

pub(crate) fn create_factory_service<Cfg, F, S, I>(
    name: String,
    tokens: Vec<(Token, SharedCfg)>,
    f: F,
) -> FactoryServiceType<Cfg>
where
    Cfg: ServerAppConfig,
    F: AsyncFn(&Cfg::Config) -> I + Send + Clone + 'static,
    I: IntoService<S, (), Io> + 'static,
    S: Service<(), Io> + 'static,
{
    Box::from(Factory {
        tokens,
        name: Arc::from(name),
        fac: Arc::new(move |cfg: Cfg::Config| {
            let f = f.clone();
            Box::pin(async move {
                let svc = (f)(&cfg).await.into_service();
                let pipeline = Pipeline::new(svc.map(|_| ()).map_err(|_| ()));
                let svc: Box<dyn NetService> = Box::new(ServerService { pipeline });
                svc
            })
        }),
    })
}

impl<Cfg: ServerAppConfig> FactoryService<Cfg> for Factory<Cfg> {
    fn clo(&self) -> FactoryServiceType<Cfg> {
        Box::new(Factory {
            name: self.name.clone(),
            tokens: self.tokens.clone(),
            fac: self.fac.clone(),
        })
    }

    fn create(&self, st: Cfg::Config) -> BoxFuture<'static, io::Result<CreateResult>> {
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
unsafe impl<Cfg: ServerAppConfig> Send for Factory<Cfg> {}
