#![allow(clippy::type_complexity)]
use std::{fmt, marker::PhantomData};

use crate::and_then::{AndThen, AndThenFactory};
use crate::apply::{Apply, ApplyCtx, ApplyFactory};
use crate::ctx::Ctx;
use crate::inspect::{Inspect, InspectErr, InspectErrFactory, InspectFactory};
use crate::map::{Map, MapFactory};
use crate::map_err::{MapErr, MapErrFactory};
use crate::map_init_err::MapInitErr;
use crate::middleware::{ApplyMiddleware, Middleware};
use crate::then::{Then, ThenFactory};
use crate::{IntoService, IntoServiceFactory, Pipeline, Service, ServiceFactory};

/// Constructs new chain with one service.
pub fn chain<S, St, Req>(service: impl IntoService<S, St, Req>) -> ServiceChain<S, St, Req>
where
    S: Service<St, Req>,
{
    ServiceChain {
        service: service.into_service(),
        _t: PhantomData,
    }
}

/// Constructs new chain factory with one service factory.
pub fn chain_factory<St, Sf, Req>(
    factory: impl IntoServiceFactory<Sf, St, Req>,
) -> ServiceChainFactory<Sf, St, Req>
where
    Sf: ServiceFactory<St, Req>,
{
    ServiceChainFactory {
        factory: factory.into_factory(),
        _t: PhantomData,
    }
}

/// Chain builder - chain allows to compose multiple service into one service.
pub struct ServiceChain<S, St, Req> {
    service: S,
    pub(crate) _t: PhantomData<(St, Req)>,
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
    pub fn and_then<Next, F>(self, service: F) -> ServiceChain<AndThen<S, Next, Req>, St, Req>
    where
        Self: Sized,
        F: IntoService<Next, St, Req>,
        Next: Service<St, Req, Error = S::Error>,
    {
        ServiceChain {
            service: AndThen::new(self.service, service.into_service()),
            _t: PhantomData,
        }
    }

    /// Chain on a computation for when a call to the service finished,
    /// passing the result of the call to the next service `U`.
    pub fn then<Next, F>(self, service: F) -> ServiceChain<Then<S, Next>, St, Req>
    where
        Self: Sized,
        F: IntoService<Next, St, Result<S::Res, S::Error>>,
        Next: Service<St, Result<S::Res, S::Error>, Error = S::Error>,
    {
        ServiceChain {
            service: Then::new(self.service, service.into_service()),
            _t: PhantomData,
        }
    }

    /// Map this service's output to a different type, returning a new service
    /// of the resulting type.
    ///
    /// This function is similar to the `Option::map` or `Iterator::map` where
    /// it will change the type of the underlying service.
    pub fn map<F, Res>(self, f: F) -> ServiceChain<Map<S, F, Res>, St, Req>
    where
        Self: Sized,
        F: Fn(S::Res) -> Res,
    {
        ServiceChain {
            service: Map::new(self.service, f),
            _t: PhantomData,
        }
    }

    /// Map this service's error to a different error, returning a new service.
    ///
    /// This function is similar to the `Result::map_err` where it will change
    /// the error type of the underlying service. This is useful for example to
    /// ensure that services have the same error type.
    pub fn map_err<F, Err>(self, f: F) -> ServiceChain<MapErr<S, F, Err>, St, Req>
    where
        Self: Sized,
        F: Fn(S::Error) -> Err,
    {
        ServiceChain {
            service: MapErr::new(self.service, f),
            _t: PhantomData,
        }
    }

    /// Calls a function with a reference to the contained value if Ok.
    ///
    /// Returns the original result.
    pub fn inspect<F>(self, f: F) -> ServiceChain<Inspect<S, F>, St, Req>
    where
        Self: Sized,
        F: Fn(&S::Res),
    {
        ServiceChain {
            service: Inspect::new(self.service, f),
            _t: PhantomData,
        }
    }

    /// Calls a function with a reference to the contained value if Err.
    ///
    /// Returns the original result.
    pub fn inspect_err<F>(self, f: F) -> ServiceChain<InspectErr<S, F>, St, Req>
    where
        Self: Sized,
        F: Fn(&S::Error),
    {
        ServiceChain {
            service: InspectErr::new(self.service, f),
            _t: PhantomData,
        }
    }

    /// Use function as middleware for current service.
    ///
    /// Short version of `apply_fn(chain(...), fn)`
    pub fn apply_fn<F, In, Out, Err>(
        self,
        f: F,
    ) -> ServiceChain<Apply<S, St, Req, F, In, Out, Err>, St, In>
    where
        F: AsyncFn(In, &ApplyCtx<'_, S, St>) -> Result<Out, Err>,
        Err: From<S::Error>,
    {
        crate::apply_fn(self.service, f)
    }

    /// Create service pipeline
    pub fn into_pipeline(self) -> Pipeline<S, St> {
        Pipeline::new(self.service)
    }
}

impl<S, St, Req> Clone for ServiceChain<S, St, Req>
where
    S: Clone,
{
    fn clone(&self) -> Self {
        ServiceChain {
            service: self.service.clone(),
            _t: PhantomData,
        }
    }
}

impl<S, St, Req> fmt::Debug for ServiceChain<S, St, Req>
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

    crate::forward_ready!(St, service);
    crate::forward_poll!(service);
    crate::forward_shutdown!(service);

    #[inline]
    async fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> Result<S::Res, S::Error> {
        ctx.call(&self.service, req).await
    }
}

/// Service factory builder
pub struct ServiceChainFactory<Sf, St, Req> {
    pub(crate) factory: Sf,
    pub(crate) _t: PhantomData<(St, Req)>,
}

impl<Sf: ServiceFactory<St, Req>, St, Req> ServiceChainFactory<Sf, St, Req> {
    /// Call another service after call to this one has resolved successfully.
    pub fn and_then<F, U>(
        self,
        factory: F,
    ) -> ServiceChainFactory<AndThenFactory<Sf, U>, St, Req>
    where
        Self: Sized,
        F: IntoServiceFactory<U, St, Sf::Res>,
        U: ServiceFactory<
                St,
                Sf::Res,
                Error = Sf::Error,
                InitCfg = Sf::InitCfg,
                InitError = Sf::InitError,
            >,
    {
        ServiceChainFactory {
            factory: AndThenFactory::new(self.factory, factory.into_factory()),
            _t: PhantomData,
        }
    }

    /// Apply Middleware to current service factory.
    ///
    /// Short version of `apply(middleware, chain_factory(...))`
    pub fn apply<U>(self, tr: U) -> ServiceChainFactory<ApplyMiddleware<U, Sf>, St, Req>
    where
        U: Middleware<Sf::Service, Sf::InitCfg>,
    {
        crate::apply(tr, self.factory)
    }

    /// Apply function middleware to current service factory.
    ///
    /// Short version of `apply_fn_factory(chain_factory(...), fn)`
    pub fn apply_fn<F, In, Out, Err>(
        self,
        f: F,
    ) -> ServiceChainFactory<ApplyFactory<Sf, St, Req, F, In, Out, Err>, St, In>
    where
        F: AsyncFn(In, &ApplyCtx<'_, Sf::Service, St>) -> Result<Out, Err> + Clone,
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
    pub fn then<F, U>(self, factory: F) -> ServiceChainFactory<ThenFactory<Sf, U>, St, Req>
    where
        Self: Sized,
        F: IntoServiceFactory<U, St, Result<Sf::Res, Sf::Error>>,
        U: ServiceFactory<
                St,
                Result<Sf::Res, Sf::Error>,
                Error = Sf::Error,
                InitCfg = Sf::InitCfg,
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
    pub fn map<F, Res>(self, f: F) -> ServiceChainFactory<MapFactory<Sf, F, Res>, St, Req>
    where
        Self: Sized,
        F: Fn(Sf::Res) -> Res + Clone,
    {
        ServiceChainFactory {
            factory: MapFactory::new(self.factory, f),
            _t: PhantomData,
        }
    }

    /// Map this service's error to a different error.
    pub fn map_err<F, E>(
        self,
        f: F,
    ) -> ServiceChainFactory<MapErrFactory<Sf, F, E>, St, Req>
    where
        Self: Sized,
        F: Fn(Sf::Error) -> E + Clone,
    {
        ServiceChainFactory {
            factory: MapErrFactory::new(self.factory, f),
            _t: PhantomData,
        }
    }

    /// Map this factory's init error to a different error, returning a new factory.
    pub fn map_init_err<F, E>(
        self,
        f: F,
    ) -> ServiceChainFactory<MapInitErr<Sf, F, E>, St, Req>
    where
        Self: Sized,
        F: Fn(Sf::InitError) -> E + Clone,
    {
        ServiceChainFactory {
            factory: MapInitErr::new(self.factory, f),
            _t: PhantomData,
        }
    }

    /// Calls a function with a reference to the contained value if Ok.
    ///
    /// Returns the original result.
    pub fn inspect<F>(self, f: F) -> ServiceChainFactory<InspectFactory<Sf, F>, St, Req>
    where
        Self: Sized,
        F: Fn(&Sf::Res) + Clone,
    {
        ServiceChainFactory {
            factory: InspectFactory::new(self.factory, f),
            _t: PhantomData,
        }
    }

    /// Calls a function with a reference to the contained value if Err.
    ///
    /// Returns the original result.
    pub fn inspect_err<F>(
        self,
        f: F,
    ) -> ServiceChainFactory<InspectErrFactory<Sf, F>, St, Req>
    where
        Self: Sized,
        F: Fn(&Sf::Error) + Clone,
    {
        ServiceChainFactory {
            factory: InspectErrFactory::new(self.factory, f),
            _t: PhantomData,
        }
    }

    /// Create and return a new service value asynchronously and wrap into a container
    pub async fn pipeline(
        &self,
        cfg: &Sf::InitCfg,
    ) -> Result<Pipeline<Sf::Service, St>, Sf::InitError>
    where
        Self: Sized,
    {
        Ok(Pipeline::new(self.factory.create(cfg).await?))
    }
}

impl<Sf, St, Req> Clone for ServiceChainFactory<Sf, St, Req>
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

impl<Sf, St, Req> fmt::Debug for ServiceChainFactory<Sf, St, Req>
where
    Sf: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServiceChainFactory")
            .field("factory", &self.factory)
            .finish()
    }
}

impl<Sf: ServiceFactory<St, Req>, St, Req> ServiceFactory<St, Req>
    for ServiceChainFactory<Sf, St, Req>
{
    type Res = Sf::Res;
    type Error = Sf::Error;
    type Service = Sf::Service;
    type InitCfg = Sf::InitCfg;
    type InitError = Sf::InitError;

    #[inline]
    async fn create(&self, cfg: &Sf::InitCfg) -> Result<Sf::Service, Sf::InitError> {
        self.factory.create(cfg).await
    }
}
