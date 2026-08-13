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
pub fn chain<Svc, St, F>(service: F) -> ServiceChain<Svc, St>
where
    Svc: Service<St>,
    F: IntoService<Svc>,
{
    ServiceChain {
        service: service.into_service(),
        _t: PhantomData,
    }
}

/// Constructs new chain factory with one service factory.
pub fn chain_factory<Fac, St, Req, F>(factory: F) -> ServiceChainFactory<Fac, St, Req>
where
    Fac: ServiceFactory<St, Req>,
    F: IntoServiceFactory<Fac, St, Req>,
{
    ServiceChainFactory {
        factory: factory.into_factory(),
        _t: PhantomData,
    }
}

/// Chain builder - chain allows to compose multiple service into one service.
pub struct ServiceChain<Svc, St> {
    service: Svc,
    pub(crate) _t: PhantomData<St>,
}

impl<Svc: Service<St>, St> ServiceChain<Svc, St> {
    /// Call another service after call to this one has resolved successfully.
    ///
    /// This function can be used to chain two services together and ensure that
    /// the second service isn't called until call to the fist service have
    /// finished. Result of the call to the first service is used as an
    /// input parameter for the second service's call.
    ///
    /// Note that this function consumes the receiving service and returns a
    /// wrapped version of it.
    pub fn and_then<Next, F>(self, service: F) -> ServiceChain<AndThen<Svc, Next>, St>
    where
        Self: Sized,
        F: IntoService<Next>,
        Next: Service<St, Req = Svc::Res, Error = Svc::Error>,
    {
        ServiceChain {
            service: AndThen::new(self.service, service.into_service()),
            _t: PhantomData,
        }
    }

    /// Chain on a computation for when a call to the service finished,
    /// passing the result of the call to the next service `U`.
    pub fn then<Next, F>(self, service: F) -> ServiceChain<Then<Svc, Next>, St>
    where
        Self: Sized,
        F: IntoService<Next>,
        Next: Service<St, Req = Result<Svc::Res, Svc::Error>, Error = Svc::Error>,
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
    pub fn map<F, Res>(self, f: F) -> ServiceChain<Map<Svc, F, Res>, St>
    where
        Self: Sized,
        F: Fn(Svc::Res) -> Res,
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
    pub fn map_err<F, Err>(self, f: F) -> ServiceChain<MapErr<Svc, F, Err>, St>
    where
        Self: Sized,
        F: Fn(Svc::Error) -> Err,
    {
        ServiceChain {
            service: MapErr::new(self.service, f),
            _t: PhantomData,
        }
    }

    /// Calls a function with a reference to the contained value if Ok.
    ///
    /// Returns the original result.
    pub fn inspect<F>(self, f: F) -> ServiceChain<Inspect<Svc, F>, St>
    where
        Self: Sized,
        F: Fn(&Svc::Res),
    {
        ServiceChain {
            service: Inspect::new(self.service, f),
            _t: PhantomData,
        }
    }

    /// Calls a function with a reference to the contained value if Err.
    ///
    /// Returns the original result.
    pub fn inspect_err<F>(self, f: F) -> ServiceChain<InspectErr<Svc, F>, St>
    where
        Self: Sized,
        F: Fn(&Svc::Error),
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
    ) -> ServiceChain<Apply<Svc, St, F, In, Out, Err>, St>
    where
        F: AsyncFn(In, &ApplyCtx<'_, Svc, St>) -> Result<Out, Err>,
        Svc: Service<St>,
        Err: From<Svc::Error>,
    {
        crate::apply_fn(self.service, f)
    }

    /// Create service pipeline
    pub fn into_pipeline(self) -> Pipeline<Svc> {
        Pipeline::new(self.service)
    }
}

impl<Svc, St> Clone for ServiceChain<Svc, St>
where
    Svc: Clone,
{
    fn clone(&self) -> Self {
        ServiceChain {
            service: self.service.clone(),
            _t: PhantomData,
        }
    }
}

impl<Svc, St> fmt::Debug for ServiceChain<Svc, St>
where
    Svc: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServiceChain")
            .field("service", &self.service)
            .finish()
    }
}

impl<Svc: Service<St>, St> Service<St> for ServiceChain<Svc, St> {
    type Req = Svc::Req;
    type Res = Svc::Res;
    type Error = Svc::Error;

    crate::forward_ready!(St, service);
    crate::forward_poll!(service);
    crate::forward_shutdown!(service);

    #[inline]
    async fn call(
        &self,
        req: Svc::Req,
        ctx: Ctx<'_, Self, St>,
    ) -> Result<Svc::Res, Svc::Error> {
        ctx.call(&self.service, req).await
    }
}

/// Service factory builder
pub struct ServiceChainFactory<Fac, St, Req> {
    pub(crate) factory: Fac,
    pub(crate) _t: PhantomData<(St, Req)>,
}

impl<Fac: ServiceFactory<St, Req>, St, Req> ServiceChainFactory<Fac, St, Req> {
    /// Call another service after call to this one has resolved successfully.
    pub fn and_then<F, U>(
        self,
        factory: F,
    ) -> ServiceChainFactory<AndThenFactory<Fac, U>, St, Req>
    where
        Self: Sized,
        F: IntoServiceFactory<U, St, Fac::Res>,
        U: ServiceFactory<
                St,
                Fac::Res,
                Error = Fac::Error,
                InitCfg = Fac::InitCfg,
                InitError = Fac::InitError,
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
    pub fn apply<U>(self, tr: U) -> ServiceChainFactory<ApplyMiddleware<U, Fac>, St, Req>
    where
        U: Middleware<Fac::Service, Fac::InitCfg>,
    {
        crate::apply(tr, self.factory)
    }

    /// Apply function middleware to current service factory.
    ///
    /// Short version of `apply_fn_factory(chain_factory(...), fn)`
    pub fn apply_fn<F, In, Out, Err>(
        self,
        f: F,
    ) -> ServiceChainFactory<ApplyFactory<Fac, St, Req, F, In, Out, Err>, St, In>
    where
        F: AsyncFn(In, &ApplyCtx<'_, Fac::Service, St>) -> Result<Out, Err> + Clone,
        Fac: ServiceFactory<St, Req>,
        Err: From<Fac::Error>,
    {
        crate::apply_fn_factory(self.factory, f)
    }

    /// Create chain factory to chain on a computation for when a call to the
    /// service finished, passing the result of the call to the next
    /// service `U`.
    ///
    /// Note that this function consumes the receiving factory and returns a
    /// wrapped version of it.
    pub fn then<F, U>(self, factory: F) -> ServiceChainFactory<ThenFactory<Fac, U>, St, Req>
    where
        Self: Sized,
        F: IntoServiceFactory<U, St, Result<Fac::Res, Fac::Error>>,
        U: ServiceFactory<
                St,
                Result<Fac::Res, Fac::Error>,
                Error = Fac::Error,
                InitCfg = Fac::InitCfg,
                InitError = Fac::InitError,
            >,
    {
        ServiceChainFactory {
            factory: ThenFactory::new(self.factory, factory.into_factory()),
            _t: PhantomData,
        }
    }

    /// Map this service's output to a different type, returning a new service
    /// of the resulting type.
    pub fn map<F, Res>(
        self,
        f: F,
    ) -> ServiceChainFactory<MapFactory<Fac, F, St, Req, Res>, St, Req>
    where
        Self: Sized,
        F: Fn(Fac::Res) -> Res + Clone,
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
    ) -> ServiceChainFactory<MapErrFactory<Fac, St, Req, F, E>, St, Req>
    where
        Self: Sized,
        F: Fn(Fac::Error) -> E + Clone,
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
    ) -> ServiceChainFactory<MapInitErr<Fac, St, Req, F, E>, St, Req>
    where
        Self: Sized,
        F: Fn(Fac::InitError) -> E + Clone,
    {
        ServiceChainFactory {
            factory: MapInitErr::new(self.factory, f),
            _t: PhantomData,
        }
    }

    /// Calls a function with a reference to the contained value if Ok.
    ///
    /// Returns the original result.
    pub fn inspect<F>(self, f: F) -> ServiceChainFactory<InspectFactory<Fac, F>, St, Req>
    where
        Self: Sized,
        F: Fn(&Fac::Res) + Clone,
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
    ) -> ServiceChainFactory<InspectErrFactory<Fac, F>, St, Req>
    where
        Self: Sized,
        F: Fn(&Fac::Error) + Clone,
    {
        ServiceChainFactory {
            factory: InspectErrFactory::new(self.factory, f),
            _t: PhantomData,
        }
    }

    /// Create and return a new service value asynchronously and wrap into a container
    pub async fn pipeline(
        &self,
        cfg: &Fac::InitCfg,
    ) -> Result<Pipeline<Fac::Service>, Fac::InitError>
    where
        Self: Sized,
    {
        Ok(Pipeline::new(self.factory.create(cfg).await?))
    }
}

impl<Fac, St, Req> Clone for ServiceChainFactory<Fac, St, Req>
where
    Fac: Clone,
{
    fn clone(&self) -> Self {
        ServiceChainFactory {
            factory: self.factory.clone(),
            _t: PhantomData,
        }
    }
}

impl<Fac, St, Req> fmt::Debug for ServiceChainFactory<Fac, St, Req>
where
    Fac: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServiceChainFactory")
            .field("factory", &self.factory)
            .finish()
    }
}

impl<Fac: ServiceFactory<St, Req>, St, Req> ServiceFactory<St, Req>
    for ServiceChainFactory<Fac, St, Req>
{
    type Res = Fac::Res;
    type Error = Fac::Error;
    type Service = Fac::Service;
    type InitCfg = Fac::InitCfg;
    type InitError = Fac::InitError;

    #[inline]
    async fn create(&self, cfg: &Fac::InitCfg) -> Result<Self::Service, Self::InitError> {
        self.factory.create(cfg).await
    }
}
