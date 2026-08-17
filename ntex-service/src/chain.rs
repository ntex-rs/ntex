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
use crate::{IntoService, IntoServiceFactory, Pipeline, Service, ServiceFactory, State};

/// Constructs new chain with one service.
pub fn unit_state<S>(service: impl IntoService<S>) -> ServiceChain<S>
where
    S: Service<St = ()>,
{
    ServiceChain {
        service: service.into_service(),
    }
}

/// Constructs new chain with one service.
pub fn chain<St, S>(service: impl IntoService<S>) -> ServiceChain<S>
where
    S: Service<St = St>,
{
    ServiceChain {
        service: service.into_service(),
    }
}

/// Constructs new chain factory with one service factory.
pub fn chain_factory<St, Sf, Req>(
    factory: impl IntoServiceFactory<Sf, Req>,
) -> ServiceChainFactory<Sf, Req>
where
    Sf: ServiceFactory<Req, St = St>,
{
    ServiceChainFactory {
        factory: factory.into_factory(),
        _t: PhantomData,
    }
}

/// Chain builder - chain allows to compose multiple service into one service.
pub struct ServiceChain<S> {
    service: S,
}

impl<S: Service> ServiceChain<S> {
    /// Call another service after call to this one has resolved successfully.
    ///
    /// This function can be used to chain two services together and ensure that
    /// the second service isn't called until call to the fist service have
    /// finished. Result of the call to the first service is used as an
    /// input parameter for the second service's call.
    ///
    /// Note that this function consumes the receiving service and returns a
    /// wrapped version of it.
    pub fn and_then<Next, F>(self, service: F) -> ServiceChain<AndThen<S, Next>>
    where
        Self: Sized,
        F: IntoService<Next>,
        Next: Service,
    {
        ServiceChain {
            service: AndThen::new(self.service, service.into_service()),
        }
    }

    /// Chain on a computation for when a call to the service finished,
    /// passing the result of the call to the next service `U`.
    pub fn then<Next, F>(self, service: F) -> ServiceChain<Then<S, Next>>
    where
        Self: Sized,
        F: IntoService<Next>,
        Next: Service<Req = Result<S::Res, S::Error>>,
    {
        ServiceChain {
            service: Then::new(self.service, service.into_service()),
        }
    }

    /// Map this service's output to a different type, returning a new service
    /// of the resulting type.
    ///
    /// This function is similar to the `Option::map` or `Iterator::map` where
    /// it will change the type of the underlying service.
    pub fn map<F, Res>(self, f: F) -> ServiceChain<Map<S, F, Res>>
    where
        Self: Sized,
        F: Fn(S::Res) -> Res,
    {
        ServiceChain {
            service: Map::new(self.service, f),
        }
    }

    /// Map this service's error to a different error, returning a new service.
    ///
    /// This function is similar to the `Result::map_err` where it will change
    /// the error type of the underlying service. This is useful for example to
    /// ensure that services have the same error type.
    pub fn map_err<F, Err>(self, f: F) -> ServiceChain<MapErr<S, F, Err>>
    where
        Self: Sized,
        F: Fn(S::Error) -> Err,
    {
        ServiceChain {
            service: MapErr::new(self.service, f),
        }
    }

    /// Calls a function with a reference to the contained value if Ok.
    ///
    /// Returns the original result.
    pub fn inspect<F>(self, f: F) -> ServiceChain<Inspect<S, F>>
    where
        Self: Sized,
        F: Fn(&S::Res),
    {
        ServiceChain {
            service: Inspect::new(self.service, f),
        }
    }

    /// Calls a function with a reference to the contained value if Err.
    ///
    /// Returns the original result.
    pub fn inspect_err<F>(self, f: F) -> ServiceChain<InspectErr<S, F>>
    where
        Self: Sized,
        F: Fn(&S::Error),
    {
        ServiceChain {
            service: InspectErr::new(self.service, f),
        }
    }

    /// Use function as middleware for current service.
    ///
    /// Short version of `apply_fn(chain(...), fn)`
    pub fn apply_fn<F, In, Out, Err>(self, f: F) -> ServiceChain<Apply<S, F, In, Out, Err>>
    where
        F: AsyncFn(In, &ApplyCtx<'_, S>) -> Result<Out, Err>,
        Err: From<S::Error>,
    {
        crate::apply_fn(self.service, f)
    }

    /// Create service pipeline
    pub fn into_pipeline(self) -> Pipeline<S>
    where
        S::St: State<S::Req>,
    {
        Pipeline::new(self.service)
    }
}

impl<S> Clone for ServiceChain<S>
where
    S: Clone,
{
    fn clone(&self) -> Self {
        ServiceChain {
            service: self.service.clone(),
        }
    }
}

impl<S> fmt::Debug for ServiceChain<S>
where
    S: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServiceChain")
            .field("service", &self.service)
            .finish()
    }
}

impl<S: Service> Service for ServiceChain<S> {
    type St = S::St;
    type Req = S::Req;
    type Res = S::Res;
    type Error = S::Error;

    crate::forward_ready!(service);
    crate::forward_shutdown!(service);

    #[inline]
    async fn call(&self, req: S::Req, ctx: Ctx<'_, Self>) -> Result<S::Res, S::Error> {
        ctx.call(&self.service, req).await
    }
}

/// Service factory builder
pub struct ServiceChainFactory<Sf, Req> {
    pub(crate) factory: Sf,
    pub(crate) _t: PhantomData<Req>,
}

impl<Sf: ServiceFactory<Req>, Req> ServiceChainFactory<Sf, Req> {
    /// Call another service after call to this one has resolved successfully.
    pub fn and_then<U>(
        self,
        factory: impl IntoServiceFactory<U, Sf::Res>,
    ) -> ServiceChainFactory<AndThenFactory<Sf, U>, Req>
    where
        Self: Sized,
        U: ServiceFactory<
                Sf::Res,
                St = Sf::St,
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
    pub fn apply<U>(self, tr: U) -> ServiceChainFactory<ApplyMiddleware<U, Sf>, Req>
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
    ) -> ServiceChainFactory<ApplyFactory<Sf, Req, F, In, Out, Err>, In>
    where
        F: AsyncFn(In, &ApplyCtx<'_, Sf::Service>) -> Result<Out, Err> + Clone,
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
    pub fn then<F, U>(self, factory: F) -> ServiceChainFactory<ThenFactory<Sf, U>, Req>
    where
        Self: Sized,
        F: IntoServiceFactory<U, Result<Sf::Res, Sf::Error>>,
        U: ServiceFactory<
                Result<Sf::Res, Sf::Error>,
                St = Sf::St,
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
    pub fn map<F, Res>(self, f: F) -> ServiceChainFactory<MapFactory<Sf, F, Res>, Req>
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
    pub fn map_err<F, E>(self, f: F) -> ServiceChainFactory<MapErrFactory<Sf, F, E>, Req>
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
    pub fn map_init_err<F, E>(self, f: F) -> ServiceChainFactory<MapInitErr<Sf, F, E>, Req>
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
    pub fn inspect<F>(self, f: F) -> ServiceChainFactory<InspectFactory<Sf, F>, Req>
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
    pub fn inspect_err<F>(self, f: F) -> ServiceChainFactory<InspectErrFactory<Sf, F>, Req>
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
    pub async fn pipeline<St>(
        &self,
        cfg: &Sf::InitCfg,
    ) -> Result<Pipeline<Sf::Service>, Sf::InitError>
    where
        St: State<Req>,
        Sf: ServiceFactory<Req, St = St> + Sized,
    {
        Ok(Pipeline::new(self.factory.create(cfg).await?))
    }
}

impl<Sf, Req> Clone for ServiceChainFactory<Sf, Req>
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

impl<Sf, Req> fmt::Debug for ServiceChainFactory<Sf, Req>
where
    Sf: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServiceChainFactory")
            .field("factory", &self.factory)
            .finish()
    }
}

impl<Sf: ServiceFactory<Req>, Req> ServiceFactory<Req> for ServiceChainFactory<Sf, Req> {
    type St = Sf::St;
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
