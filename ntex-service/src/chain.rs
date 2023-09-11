#![allow(clippy::type_complexity)]
use std::{fmt, future::Future, marker::PhantomData};

use crate::and_then::{AndThen, AndThenFactory};
use crate::apply::{Apply, ApplyFactory};
use crate::ctx::{ServiceCall, ServiceCtx};
use crate::map::{Map, MapFactory};
use crate::map_err::{MapErr, MapErrFactory};
use crate::map_init_err::MapInitErr;
use crate::middleware::{ApplyMiddleware, Middleware};
use crate::pipeline::CreatePipeline;
use crate::then::{Then, ThenFactory};
use crate::{IntoService, IntoServiceFactory, Pipeline, Service, ServiceFactory};

/// Constructs new pipeline with one service in pipeline chain.
pub fn chain<Svc, Req, F>(service: F) -> ServiceChain<Svc, Req>
where
    Svc: Service<Req>,
    F: IntoService<Svc, Req>,
{
    ServiceChain {
        service: service.into_service(),
        _t: PhantomData,
    }
}

/// Constructs new pipeline factory with one service factory.
pub fn chain_factory<T, R, C, F>(factory: F) -> ServiceChainFactory<T, R, C>
where
    T: ServiceFactory<R, C>,
    F: IntoServiceFactory<T, R, C>,
{
    ServiceChainFactory {
        factory: factory.into_factory(),
        _t: PhantomData,
    }
}

/// Pipeline builder - pipeline allows to compose multiple service into one service.
pub struct ServiceChain<Svc, Req> {
    service: Svc,
    _t: PhantomData<Req>,
}

impl<Svc: Service<Req>, Req> ServiceChain<Svc, Req> {
    /// Call another service after call to this one has resolved successfully.
    ///
    /// This function can be used to chain two services together and ensure that
    /// the second service isn't called until call to the fist service have
    /// finished. Result of the call to the first service is used as an
    /// input parameter for the second service's call.
    ///
    /// Note that this function consumes the receiving service and returns a
    /// wrapped version of it.
    pub fn and_then<Next, F>(self, service: F) -> ServiceChain<AndThen<Svc, Next>, Req>
    where
        Self: Sized,
        F: IntoService<Next, Svc::Response>,
        Next: Service<Svc::Response, Error = Svc::Error>,
    {
        ServiceChain {
            service: AndThen::new(self.service, service.into_service()),
            _t: PhantomData,
        }
    }

    /// Chain on a computation for when a call to the service finished,
    /// passing the result of the call to the next service `U`.
    ///
    /// Note that this function consumes the receiving pipeline and returns a
    /// wrapped version of it.
    pub fn then<Next, F>(self, service: F) -> ServiceChain<Then<Svc, Next>, Req>
    where
        Self: Sized,
        F: IntoService<Next, Result<Svc::Response, Svc::Error>>,
        Next: Service<Result<Svc::Response, Svc::Error>, Error = Svc::Error>,
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
    ///
    /// Note that this function consumes the receiving service and returns a
    /// wrapped version of it, similar to the existing `map` methods in the
    /// standard library.
    pub fn map<F, Res>(self, f: F) -> ServiceChain<Map<Svc, F, Req, Res>, Req>
    where
        Self: Sized,
        F: Fn(Svc::Response) -> Res,
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
    ///
    /// Note that this function consumes the receiving service and returns a
    /// wrapped version of it.
    pub fn map_err<F, Err>(self, f: F) -> ServiceChain<MapErr<Svc, F, Err>, Req>
    where
        Self: Sized,
        F: Fn(Svc::Error) -> Err,
    {
        ServiceChain {
            service: MapErr::new(self.service, f),
            _t: PhantomData,
        }
    }

    /// Use function as middleware for current service.
    ///
    /// Short version of `apply_fn(chain(...), fn)`
    pub fn apply_fn<F, R, In, Out, Err>(
        self,
        f: F,
    ) -> ServiceChain<Apply<Svc, Req, F, R, In, Out, Err>, In>
    where
        F: Fn(In, Pipeline<Svc>) -> R,
        R: Future<Output = Result<Out, Err>>,
        Svc: Service<Req, Error = Err>,
    {
        ServiceChain {
            service: Apply::new(self.service, f),
            _t: PhantomData,
        }
    }

    /// Create service pipeline
    pub fn pipeline(self) -> Pipeline<Svc> {
        Pipeline::new(self.service)
    }
}

impl<Svc, Req> Clone for ServiceChain<Svc, Req>
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

impl<Svc, Req> fmt::Debug for ServiceChain<Svc, Req>
where
    Svc: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServiceChain")
            .field("service", &self.service)
            .finish()
    }
}

impl<Svc: Service<Req>, Req> Service<Req> for ServiceChain<Svc, Req> {
    type Response = Svc::Response;
    type Error = Svc::Error;
    type Future<'f> = ServiceCall<'f, Svc, Req> where Self: 'f, Req: 'f;

    crate::forward_poll_ready!(service);
    crate::forward_poll_shutdown!(service);

    #[inline]
    fn call<'a>(&'a self, req: Req, ctx: ServiceCtx<'a, Self>) -> Self::Future<'a> {
        ctx.call(&self.service, req)
    }
}

/// Service factory builder
pub struct ServiceChainFactory<T, Req, C = ()> {
    factory: T,
    _t: PhantomData<(Req, C)>,
}

impl<T: ServiceFactory<Req, C>, Req, C> ServiceChainFactory<T, Req, C> {
    /// Call another service after call to this one has resolved successfully.
    pub fn and_then<F, U>(
        self,
        factory: F,
    ) -> ServiceChainFactory<AndThenFactory<T, U>, Req, C>
    where
        Self: Sized,
        F: IntoServiceFactory<U, T::Response, C>,
        U: ServiceFactory<T::Response, C, Error = T::Error, InitError = T::InitError>,
    {
        ServiceChainFactory {
            factory: AndThenFactory::new(self.factory, factory.into_factory()),
            _t: PhantomData,
        }
    }

    /// Apply middleware to current service factory.
    ///
    /// Short version of `apply(middleware, chain_factory(...))`
    pub fn apply<U>(self, tr: U) -> ServiceChainFactory<ApplyMiddleware<U, T, C>, Req, C>
    where
        U: Middleware<T::Service>,
    {
        ServiceChainFactory {
            factory: ApplyMiddleware::new(tr, self.factory),
            _t: PhantomData,
        }
    }

    /// Apply function middleware to current service factory.
    ///
    /// Short version of `apply_fn_factory(chain_factory(...), fn)`
    pub fn apply_fn<F, R, In, Out, Err>(
        self,
        f: F,
    ) -> ServiceChainFactory<ApplyFactory<T, Req, C, F, R, In, Out, Err>, In, C>
    where
        F: Fn(In, Pipeline<T::Service>) -> R + Clone,
        R: Future<Output = Result<Out, Err>>,
        T: ServiceFactory<Req, C, Error = Err>,
    {
        ServiceChainFactory {
            factory: ApplyFactory::new(self.factory, f),
            _t: PhantomData,
        }
    }

    /// Create `NewService` to chain on a computation for when a call to the
    /// service finished, passing the result of the call to the next
    /// service `U`.
    ///
    /// Note that this function consumes the receiving pipeline and returns a
    /// wrapped version of it.
    pub fn then<F, U>(self, factory: F) -> ServiceChainFactory<ThenFactory<T, U>, Req, C>
    where
        Self: Sized,
        C: Clone,
        F: IntoServiceFactory<U, Result<T::Response, T::Error>, C>,
        U: ServiceFactory<
            Result<T::Response, T::Error>,
            C,
            Error = T::Error,
            InitError = T::InitError,
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
    ) -> ServiceChainFactory<MapFactory<T, F, Req, Res, C>, Req, C>
    where
        Self: Sized,
        F: Fn(T::Response) -> Res + Clone,
    {
        ServiceChainFactory {
            factory: MapFactory::new(self.factory, f),
            _t: PhantomData,
        }
    }

    /// Map this service's error to a different error, returning a new service.
    pub fn map_err<F, E>(
        self,
        f: F,
    ) -> ServiceChainFactory<MapErrFactory<T, Req, C, F, E>, Req, C>
    where
        Self: Sized,
        F: Fn(T::Error) -> E + Clone,
    {
        ServiceChainFactory {
            factory: MapErrFactory::new(self.factory, f),
            _t: PhantomData,
        }
    }

    /// Map this factory's init error to a different error, returning a new service.
    pub fn map_init_err<F, E>(
        self,
        f: F,
    ) -> ServiceChainFactory<MapInitErr<T, Req, C, F, E>, Req, C>
    where
        Self: Sized,
        F: Fn(T::InitError) -> E + Clone,
    {
        ServiceChainFactory {
            factory: MapInitErr::new(self.factory, f),
            _t: PhantomData,
        }
    }

    /// Create and return a new service value asynchronously and wrap into a container
    pub fn pipeline(&self, cfg: C) -> CreatePipeline<'_, T, Req, C>
    where
        Self: Sized,
    {
        CreatePipeline::new(self.factory.create(cfg))
    }
}

impl<T, R, C> Clone for ServiceChainFactory<T, R, C>
where
    T: Clone,
{
    fn clone(&self) -> Self {
        ServiceChainFactory {
            factory: self.factory.clone(),
            _t: PhantomData,
        }
    }
}

impl<T, R, C> fmt::Debug for ServiceChainFactory<T, R, C>
where
    T: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServiceChainFactory")
            .field("factory", &self.factory)
            .finish()
    }
}

impl<T: ServiceFactory<R, C>, R, C> ServiceFactory<R, C> for ServiceChainFactory<T, R, C> {
    type Response = T::Response;
    type Error = T::Error;
    type Service = T::Service;
    type InitError = T::InitError;
    type Future<'f> = T::Future<'f> where Self: 'f;

    #[inline]
    fn create(&self, cfg: C) -> Self::Future<'_> {
        self.factory.create(cfg)
    }
}
