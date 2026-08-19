use std::{fmt, marker::PhantomData};

use crate::and_then::{AndThen, AndThenFactory};
use crate::apply::{Apply, ApplyCtx, ApplyFactory};
use crate::ctx::Ctx;
use crate::map::{Map, MapFactory};
use crate::map_err::{MapErr, MapErrFactory};
use crate::map_init_err::MapInitErr;
use crate::middleware::{ApplyMiddleware, Middleware};
use crate::then::{Then, ThenFactory};
use crate::{IntoService, IntoServiceFactory, Pipeline, Service, ServiceFactory};

/// Constructs new chain with one service.
pub fn svc<S, St>(service: impl IntoService<S, St>) -> ServiceChain<S, St>
where
    S: Service<St>,
{
    ServiceChain {
        service: service.into_service(),
        st: PhantomData,
    }
}

/// Constructs new chain factory with one service factory.
pub fn factory<Sf, Req>(
    factory: impl IntoServiceFactory<Sf, (), Req>,
) -> ServiceChainFactory<Sf, (), Req>
where
    Sf: ServiceFactory<(), Req>,
{
    ServiceChainFactory {
        factory: factory.into_factory(),
        _t: PhantomData,
    }
}

/// Constructs new chain factory with one service factory.
pub fn factory_with_st<Sf, St, Req>(
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
pub struct ServiceChain<S, St> {
    service: S,
    st: PhantomData<St>,
}

impl<S: Service<St>, St> ServiceChain<S, St> {
    /// Call another service after call to this one has resolved successfully.
    ///
    /// This function can be used to chain two services together and ensure that
    /// the second service isn't called until call to the fist service have
    /// finished. Result of the call to the first service is used as an
    /// input parameter for the second service's call.
    ///
    /// Note that this function consumes the receiving service and returns a
    /// wrapped version of it.
    pub fn and_then<Next, F>(self, service: F) -> ServiceChain<AndThen<S, Next>, St>
    where
        Self: Sized,
        F: IntoService<Next, St>,
        Next: Service<St>,
    {
        ServiceChain {
            service: AndThen::new(self.service, service.into_service()),
            st: PhantomData,
        }
    }

    /// Chain on a computation for when a call to the service finished,
    /// passing the result of the call to the next service `U`.
    pub fn then<Next, F>(self, service: F) -> ServiceChain<Then<S, Next>, St>
    where
        Self: Sized,
        F: IntoService<Next, St>,
        Next: Service<St, Req = Result<S::Res, S::Error>>,
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
    pub fn map<F, Res>(self, f: F) -> ServiceChain<Map<S, F, Res>, St>
    where
        Self: Sized,
        F: Fn(S::Res) -> Res,
    {
        ServiceChain {
            service: Map::new(self.service, f),
            st: PhantomData,
        }
    }

    /// Map this service's error to a different error, returning a new service.
    ///
    /// This function is similar to the `Result::map_err` where it will change
    /// the error type of the underlying service. This is useful for example to
    /// ensure that services have the same error type.
    pub fn map_err<F, Err>(self, f: F) -> ServiceChain<MapErr<S, F, Err>, St>
    where
        Self: Sized,
        F: Fn(S::Error) -> Err,
    {
        ServiceChain {
            service: MapErr::new(self.service, f),
            st: PhantomData,
        }
    }

    /// Use function as middleware for current service.
    ///
    /// Short version of `apply_fn(svc(...), fn)`
    pub fn apply_fn<F, In, Out, Err>(self, f: F) -> ServiceChain<Apply<S, St, F, In, Out, Err>, St>
    where
        F: AsyncFn(In, &ApplyCtx<'_, S, St>) -> Result<Out, Err>,
        Err: From<S::Error>,
    {
        crate::apply_fn(self.service, f)
    }

    /// Create service pipeline
    pub fn into_pipeline(self) -> Pipeline<S::Req, S::Res, S::Error>
    where
        S: 'static,
        // St: State<S::Req> + Default + 'static,
        St: Default + 'static,
    {
        Pipeline::with(self.service)
    }
}

impl<S: Service<St>, St> Clone for ServiceChain<S, St>
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

impl<S: Service<St>, St> fmt::Debug for ServiceChain<S, St>
where
    S: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServiceChain")
            .field("service", &self.service)
            .finish()
    }
}

impl<S: Service<St>, St> Service<St> for ServiceChain<S, St> {
    type Req = S::Req;
    type Res = S::Res;
    type Error = S::Error;

    crate::forward_ready!(St, service);
    crate::forward_shutdown!(service);

    #[inline]
    async fn call(&self, req: Self::Req, ctx: Ctx<'_, Self, St>) -> Result<Self::Res, Self::Error> {
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
    pub fn and_then<U>(
        self,
        factory: impl IntoServiceFactory<U, St, Sf::Res>,
    ) -> ServiceChainFactory<AndThenFactory<Sf, U>, St, Req>
    where
        Self: Sized,
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
    /// Short version of `apply(middleware, factory(...))`
    pub fn apply<U>(self, tr: U) -> ServiceChainFactory<ApplyMiddleware<U, Sf>, St, Req>
    where
        U: Middleware<Sf::Service, Sf::InitCfg>,
    {
        crate::apply(tr, self.factory)
    }

    /// Apply function middleware to current service factory.
    ///
    /// Short version of `apply_fn_factory(factory(...), fn)`
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
    pub fn map_err<F, E>(self, f: F) -> ServiceChainFactory<MapErrFactory<Sf, F, E>, St, Req>
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
    pub fn map_init_err<F, E>(self, f: F) -> ServiceChainFactory<MapInitErr<Sf, F, E>, St, Req>
    where
        Self: Sized,
        F: Fn(Sf::InitError) -> E + Clone,
    {
        ServiceChainFactory {
            factory: MapInitErr::new(self.factory, f),
            _t: PhantomData,
        }
    }

    /// Create and return a new service value asynchronously and wrap into a container
    pub async fn pipeline(
        &self,
        cfg: &Sf::InitCfg,
    ) -> Result<Pipeline<Req, Sf::Res, Sf::Error>, Sf::InitError>
    where
        Sf: 'static,
        //St: State<Req> + Default + 'static,
        St: Default + 'static,
        Req: 'static,
    {
        Ok(Pipeline::with(self.factory.create(cfg).await?))
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
