use std::{io, marker::PhantomData, sync::Arc};

use ntex_io::Io;
use ntex_service::{BindCtx, IntoService, Pipeline, Service, cfg::SharedCfg};
use ntex_util::future::BoxFuture;

use super::{ServerAppConfig, Token, service::ConnGuard};

pub(super) type FactoryServiceType<St> = Box<dyn FactoryService<St>>;
type CreateResult = Vec<(Box<dyn NetService>, Arc<str>, Vec<(Token, SharedCfg)>)>;

pub(crate) trait NetService {
    fn bind(&self, ctx: &BindCtx<'_>);

    fn call(&self, _: Io, _: ConnGuard);

    fn ready(&self) -> BoxFuture<'_, Result<(), ()>>;

    fn shutdown(&self) -> BoxFuture<'_, ()>;
}

pub(crate) trait FactoryService<Cfg: ServerAppConfig>: Send {
    fn clo(&self) -> FactoryServiceType<Cfg>;

    fn create(&self, st: Cfg::State) -> BoxFuture<'static, io::Result<CreateResult>>;
}

// Each instance owns its factory, `F` is cloned, never shared between threads
struct Factory<Cfg, F, S> {
    name: Arc<str>,
    tokens: Vec<(Token, SharedCfg)>,
    f: F,
    _t: PhantomData<fn() -> (Cfg, S)>,
}

pub(crate) fn create_factory_service<Cfg, F, S, I>(
    name: String,
    tokens: Vec<(Token, SharedCfg)>,
    f: F,
) -> FactoryServiceType<Cfg>
where
    Cfg: ServerAppConfig,
    F: AsyncFn(&Cfg::State) -> I + Send + Clone + 'static,
    I: IntoService<S, Cfg::State, Io> + 'static,
    S: Service<Cfg::State, Io> + 'static,
{
    Box::new(Factory {
        tokens,
        name: Arc::from(name),
        f,
        _t: PhantomData::<fn() -> (Cfg, S)>,
    })
}

impl<Cfg, F, S, I> FactoryService<Cfg> for Factory<Cfg, F, S>
where
    Cfg: ServerAppConfig,
    F: AsyncFn(&Cfg::State) -> I + Send + Clone + 'static,
    I: IntoService<S, Cfg::State, Io> + 'static,
    S: Service<Cfg::State, Io> + 'static,
{
    fn clo(&self) -> FactoryServiceType<Cfg> {
        Box::new(Factory {
            name: self.name.clone(),
            tokens: self.tokens.clone(),
            f: self.f.clone(),
            _t: PhantomData::<fn() -> (Cfg, S)>,
        })
    }

    fn create(&self, st: Cfg::State) -> BoxFuture<'static, io::Result<CreateResult>> {
        let name = self.name.clone();
        let tokens = self.tokens.clone();
        let f = self.f.clone();

        Box::pin(async move {
            let svc = f(&st).await.into_service();
            let pipeline = Pipeline::new(st, svc.map(|_| ()).map_err(|_| ()));
            let svc: Box<dyn NetService> = Box::new(ServerService { pipeline });
            Ok(vec![(svc, name, tokens)])
        })
    }
}

pub(crate) struct ServerService {
    pub(crate) pipeline: Pipeline<Io, (), ()>,
}

impl NetService for ServerService {
    fn bind(&self, ctx: &BindCtx<'_>) {
        self.pipeline.bind_to(ctx);
    }

    fn call(&self, io: Io, guard: ConnGuard) {
        let fut = self.pipeline.call_static(io);
        ntex_rt::spawn_detached(async move {
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

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    // Send but not Sync, clone mutates the source
    struct CloneCounter(Cell<usize>);

    impl Clone for CloneCounter {
        fn clone(&self) -> Self {
            self.0.set(self.0.get() + 1);
            Self(Cell::new(0))
        }
    }

    impl CloneCounter {
        fn touch(&self) -> usize {
            self.0.get()
        }
    }

    // Under miri, a factory shared between instances is a data race
    #[test]
    fn factory_not_shared_between_threads() {
        let counter = CloneCounter(Cell::new(0));
        let first = create_factory_service::<crate::NoConfig, _, _, _>(
            "test".to_owned(),
            Vec::new(),
            async move |(): &()| {
                let _ = counter.touch();
                ntex_service::fn_service(async |_: Io| Ok::<_, ()>(()))
            },
        );
        let second = first.clo();
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            for fac in [first, second] {
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    drop(fac.create(()));
                });
            }
        });
    }
}
