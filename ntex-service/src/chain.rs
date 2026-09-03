use std::{fmt, marker::PhantomData};

use crate::and_then::{AndThen, AndThenFactory};
use crate::apply::{Apply, ApplyCtx, ApplyFactory};
use crate::ctx::Ctx;
use crate::fn_ready::FnReadiness;
use crate::fn_shutdown::FnShutdown;
use crate::map::{Map, MapFactory};
use crate::map_err::{MapErr, MapErrFactory};
use crate::map_init_err::MapInitErr;
use crate::middleware::{ApplyMiddleware, Middleware};
use crate::pipeline::Pipeline;
use crate::then::{Then, ThenFactory};
use crate::{IntoService, IntoServiceFactory, Service, ServiceFactory};

/// Constructs new chain with one service.
pub fn svc<S, St, Req>(service: impl IntoService<S, St, Req>) -> ServiceChain<S, St, Req>
where
    S: Service<St, Req>,
{
    ServiceChain {
        service: service.into_service(),
        st: PhantomData,
    }
}

/// Constructs new chain with one service.
pub fn service<S, St, Req>(service: impl IntoService<S, St, Req>) -> ServiceChain<S, St, Req>
where
    S: Service<St, Req>,
{
    ServiceChain {
        service: service.into_service(),
        st: PhantomData,
    }
}

/// Constructs new chain factory with one service factory.
pub fn factory<Sf, St, Req, Cfg>(
    factory: impl IntoServiceFactory<Sf, St, Req, Cfg>,
) -> ServiceChainFactory<Sf, St, Req, Cfg>
where
    Sf: ServiceFactory<St, Req, Cfg>,
{
    ServiceChainFactory {
        factory: factory.into_factory(),
        _t: PhantomData,
    }
}

/// Constructs new chain factory with one service factory.
pub fn factory_no_st<Sf, Req, Cfg>(
    factory: impl IntoServiceFactory<Sf, (), Req, Cfg>,
) -> ServiceChainFactory<Sf, (), Req, Cfg>
where
    Sf: ServiceFactory<(), Req, Cfg>,
{
    ServiceChainFactory {
        factory: factory.into_factory(),
        _t: PhantomData,
    }
}

/// Chain builder - chain allows to compose multiple service into one service.
pub struct ServiceChain<S, St, Req> {
    service: S,
    st: PhantomData<(St, Req)>,
}

/// Service factory builder
pub struct ServiceChainFactory<Sf, St, Req, Cfg> {
    pub(crate) factory: Sf,
    pub(crate) _t: PhantomData<(St, Req, Cfg)>,
}

impl<S: Service<St, Req>, St, Req> ServiceChain<S, St, Req> {
    /// Call another service after call to this one has resolved successfully.
    ///
    /// This function can be used to chain two services together and ensure that
    /// the second service isn't called until call to the fist service have
    /// finished. Result of the call to the first service is used as an
    /// input parameter for the second service's call.
    ///
    /// Note that this function consumes the receiving service and returns a
    /// wrapped version of it.
    pub fn and_then<Next, F>(self, service: F) -> ServiceChain<AndThen<S, Next>, St, Req>
    where
        Self: Sized,
        F: IntoService<Next, St, S::Res>,
        Next: Service<St, S::Res>,
    {
        ServiceChain {
            service: AndThen::new(self.service, service.into_service()),
            st: PhantomData,
        }
    }

    /// Chain on a computation for when a call to the service finished,
    /// passing the result of the call to the next service `U`.
    pub fn then<Next, F>(self, service: F) -> ServiceChain<Then<S, Next>, St, Req>
    where
        Self: Sized,
        F: IntoService<Next, St, Result<S::Res, S::Error>>,
        Next: Service<St, Result<S::Res, S::Error>>,
    {
        ServiceChain {
            service: Then::new(self.service, service.into_service()),
            st: PhantomData,
        }
    }

    /// Map this service's output to a different type, returning a new service
    /// of the resulting type.
    ///
    /// This function is similar to the `Option::map` or `Iterator::map` where
    /// it will change the type of the underlying service.
    pub fn map<F, Res>(self, f: F) -> ServiceChain<Map<F, S, Res>, St, Req>
    where
        Self: Sized,
        F: Fn(S::Res) -> Res,
    {
        ServiceChain {
            service: Map::new(f, self.service),
            st: PhantomData,
        }
    }

    /// Map this service's error to a different error, returning a new service.
    ///
    /// This function is similar to the `Result::map_err` where it will change
    /// the error type of the underlying service. This is useful for example to
    /// ensure that services have the same error type.
    pub fn map_err<F, Err>(self, f: F) -> ServiceChain<MapErr<F, S, Err>, St, Req>
    where
        Self: Sized,
        F: Fn(S::Error) -> Err,
    {
        ServiceChain {
            service: MapErr::new(f, self.service),
            st: PhantomData,
        }
    }

    /// Add custom readiness check to the service chain.
    pub fn readiness<F>(
        self,
        ready: F,
    ) -> ServiceChain<AndThen<S, FnReadiness<F, S::Error>>, St, Req>
    where
        Self: Sized,
        F: AsyncFn(&St) -> Result<(), S::Error>,
    {
        ServiceChain {
            service: AndThen::new(self.service, FnReadiness::new(ready)),
            st: PhantomData,
        }
    }

    /// Add custom readiness check to the service chain.
    pub fn shutdown<F>(self, sh: F) -> ServiceChain<AndThen<S, FnShutdown<F, S::Error>>, St, Req>
    where
        Self: Sized,
        F: AsyncFnOnce(&St),
    {
        ServiceChain {
            service: AndThen::new(self.service, FnShutdown::new(sh)),
            st: PhantomData,
        }
    }

    /// Use function as middleware for current service.
    ///
    /// Short version of `apply_fn(service(...), fn)`
    pub fn apply_fn<F, In, Out, Err>(
        self,
        f: F,
    ) -> ServiceChain<Apply<S, St, Req, F, In, Out, Err>, St, In>
    where
        F: AsyncFn(In, &ApplyCtx<'_, S, St, Req>) -> Result<Out, Err>,
        Err: From<S::Error>,
    {
        crate::apply_fn(self.service, f)
    }

    /// Create service pipeline
    pub fn into_pipeline(self) -> Pipeline<Req, S::Res, S::Error>
    where
        S: 'static,
        St: Default + 'static,
        Req: 'static,
    {
        Pipeline::new(self.service)
    }
}

impl<S: Service<St, Req>, St, Req> Clone for ServiceChain<S, St, Req>
where
    S: Clone,
{
    fn clone(&self) -> Self {
        ServiceChain {
            service: self.service.clone(),
            st: PhantomData,
        }
    }
}

impl<S: Service<St, Req>, St, Req> fmt::Debug for ServiceChain<S, St, Req>
where
    S: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServiceChain")
            .field("service", &self.service)
            .finish()
    }
}

impl<S: Service<St, Req>, St, Req> Service<St, Req> for ServiceChain<S, St, Req> {
    type Res = S::Res;
    type Error = S::Error;

    #[inline]
    async fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> Result<Self::Res, Self::Error> {
        ctx.call(&self.service, req).await
    }

    crate::forward_ready!(St, service);
    crate::forward_shutdown!(St, service);
}

impl<Sf: ServiceFactory<St, Req, Cfg>, St, Req, Cfg> ServiceChainFactory<Sf, St, Req, Cfg> {
    /// Call another service after call to this one has resolved successfully.
    pub fn and_then<U>(
        self,
        factory: impl IntoServiceFactory<U, St, Sf::Res, Cfg>,
    ) -> ServiceChainFactory<AndThenFactory<Sf, U>, St, Req, Cfg>
    where
        Self: Sized,
        U: ServiceFactory<St, Sf::Res, Cfg, Error = Sf::Error, InitError = Sf::InitError>,
    {
        ServiceChainFactory {
            factory: AndThenFactory::new(self.factory, factory.into_factory()),
            _t: PhantomData,
        }
    }

    /// Apply Middleware to current service factory.
    ///
    /// Short version of `apply(middleware, factory(...))`
    pub fn apply<U>(self, tr: U) -> ServiceChainFactory<ApplyMiddleware<U, Sf>, St, Req, Cfg>
    where
        U: Middleware<Sf::Service, St, Cfg>,
    {
        crate::apply(tr, self.factory)
    }

    /// Apply function middleware to current service factory.
    ///
    /// Short version of `apply_fn_factory(factory(...), fn)`
    pub fn apply_fn<F, In, Out, Err>(
        self,
        f: F,
    ) -> ServiceChainFactory<ApplyFactory<F, Sf, St, Req, Cfg, In, Out, Err>, St, In, Cfg>
    where
        F: AsyncFn(In, &ApplyCtx<'_, Sf::Service, St, Req>) -> Result<Out, Err> + Clone,
        Err: From<Sf::Error>,
    {
        crate::apply_fn_factory(self.factory, f)
    }

    /// Create chain factory to chain on a computation for when a call to the
    /// service finished, passing the result of the call to the next
    /// service `U`.
    ///
    /// Note that this function consumes the receiving factory and returns a
    /// wrapped version of it.
    pub fn then<F, U>(self, factory: F) -> ServiceChainFactory<ThenFactory<Sf, U>, St, Req, Cfg>
    where
        Self: Sized,
        F: IntoServiceFactory<U, St, Result<Sf::Res, Sf::Error>, Cfg>,
        U: ServiceFactory<
                St,
                Result<Sf::Res, Sf::Error>,
                Cfg,
                Error = Sf::Error,
                InitError = Sf::InitError,
            >,
    {
        ServiceChainFactory {
            factory: ThenFactory::new(self.factory, factory.into_factory()),
            _t: PhantomData,
        }
    }

    /// Map this service's output to a different type, returning a new service
    /// of the resulting type.
    pub fn map<F, Res>(self, f: F) -> ServiceChainFactory<MapFactory<F, Sf, Res>, St, Req, Cfg>
    where
        Self: Sized,
        F: Fn(Sf::Res) -> Res + Clone,
    {
        ServiceChainFactory {
            factory: MapFactory::new(f, self.factory),
            _t: PhantomData,
        }
    }

    /// Map this service's error to a different error.
    pub fn map_err<F, E>(self, f: F) -> ServiceChainFactory<MapErrFactory<F, Sf, E>, St, Req, Cfg>
    where
        Self: Sized,
        F: Fn(Sf::Error) -> E + Clone,
    {
        ServiceChainFactory {
            factory: MapErrFactory::new(f, self.factory),
            _t: PhantomData,
        }
    }

    /// Map this factory's init error to a different error, returning a new factory.
    pub fn map_init_err<F, E>(self, f: F) -> ServiceChainFactory<MapInitErr<F, Sf, E>, St, Req, Cfg>
    where
        Self: Sized,
        F: Fn(Sf::InitError) -> E + Clone,
    {
        ServiceChainFactory {
            factory: MapInitErr::new(f, self.factory),
            _t: PhantomData,
        }
    }

    /// Add custom readiness check to the service factory.
    pub fn readiness<F>(
        self,
        ready: F,
    ) -> ServiceChainFactory<AndThenFactory<Sf, FnReadiness<F, Sf::Error>>, St, Req, Cfg>
    where
        Self: Sized,
        F: AsyncFn(&St) -> Result<(), Sf::Error> + Clone,
    {
        ServiceChainFactory {
            factory: AndThenFactory::new(self.factory, FnReadiness::new(ready)),
            _t: PhantomData,
        }
    }

    /// Add custom shutdown callback to the service factory.
    pub fn shutdown<F>(
        self,
        sh: F,
    ) -> ServiceChainFactory<AndThenFactory<Sf, FnShutdown<F, Sf::Error>>, St, Req, Cfg>
    where
        Self: Sized,
        F: AsyncFnOnce(&St) + Clone,
    {
        ServiceChainFactory {
            factory: AndThenFactory::new(self.factory, FnShutdown::new(sh)),
            _t: PhantomData,
        }
    }

    /// Create and return a new service value asynchronously and wrap into a container
    pub async fn pipeline(
        &self,
        cfg: &Cfg,
    ) -> Result<Pipeline<Req, Sf::Res, Sf::Error>, Sf::InitError>
    where
        Sf: 'static,
        St: Default + 'static,
        Req: 'static,
        Cfg: 'static,
    {
        Ok(Pipeline::new(self.factory.create(cfg).await?))
    }
}

impl<Sf, St, Req, Cfg> Clone for ServiceChainFactory<Sf, St, Req, Cfg>
where
    Sf: Clone,
{
    fn clone(&self) -> Self {
        ServiceChainFactory {
            factory: self.factory.clone(),
            _t: PhantomData,
        }
    }
}

impl<Sf, St, Req, Cfg> fmt::Debug for ServiceChainFactory<Sf, St, Req, Cfg>
where
    Sf: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServiceChainFactory")
            .field("factory", &self.factory)
            .finish()
    }
}

impl<Sf: ServiceFactory<St, Req, Cfg>, St, Req, Cfg> ServiceFactory<St, Req, Cfg>
    for ServiceChainFactory<Sf, St, Req, Cfg>
{
    type Res = Sf::Res;
    type Error = Sf::Error;
    type Service = Sf::Service;
    type InitError = Sf::InitError;

    #[inline]
    async fn create(&self, cfg: &Cfg) -> Result<Sf::Service, Sf::InitError> {
        self.factory.create(cfg).await
    }
}
