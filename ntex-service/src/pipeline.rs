use std::marker::PhantomData;

use crate::and_then::{AndThen, AndThenFactory};
use crate::ctx::{Container, ServiceCall, ServiceCtx};
use crate::map::{Map, MapFactory};
use crate::map_err::{MapErr, MapErrFactory};
use crate::map_init_err::MapInitErr;
use crate::middleware::{ApplyMiddleware, Middleware};
use crate::then::{Then, ThenFactory};
use crate::{IntoService, IntoServiceFactory, Service, ServiceFactory};

/// Constructs new pipeline with one service in pipeline chain.
pub fn pipeline<Svc, Req, F>(service: F) -> Pipeline<Req, Svc>
where
    Svc: Service<Req>,
    F: IntoService<Svc, Req>,
{
    Pipeline {
        service: service.into_service(),
        _t: PhantomData,
    }
}

/// Constructs new pipeline factory with one service factory.
pub fn pipeline_factory<T, R, C, F>(factory: F) -> PipelineFactory<R, T, C>
where
    T: ServiceFactory<R, C>,
    F: IntoServiceFactory<T, R, C>,
{
    PipelineFactory {
        factory: factory.into_factory(),
        _t: PhantomData,
    }
}

/// Pipeline service - pipeline allows to compose multiple service into one service.
pub struct Pipeline<Req, Svc> {
    service: Svc,
    _t: PhantomData<Req>,
}

impl<Req, Svc: Service<Req>> Pipeline<Req, Svc> {
    /// Call another service after call to this one has resolved successfully.
    ///
    /// This function can be used to chain two services together and ensure that
    /// the second service isn't called until call to the fist service have
    /// finished. Result of the call to the first service is used as an
    /// input parameter for the second service's call.
    ///
    /// Note that this function consumes the receiving service and returns a
    /// wrapped version of it.
    pub fn and_then<Next, F>(self, service: F) -> Pipeline<Req, AndThen<Svc, Next>>
    where
        Self: Sized,
        F: IntoService<Next, Svc::Response>,
        Next: Service<Svc::Response, Error = Svc::Error>,
    {
        Pipeline {
            service: AndThen::new(self.service, service.into_service()),
            _t: PhantomData,
        }
    }

    /// Chain on a computation for when a call to the service finished,
    /// passing the result of the call to the next service `U`.
    ///
    /// Note that this function consumes the receiving pipeline and returns a
    /// wrapped version of it.
    pub fn then<Next, F>(self, service: F) -> Pipeline<Req, Then<Svc, Next>>
    where
        Self: Sized,
        F: IntoService<Next, Result<Svc::Response, Svc::Error>>,
        Next: Service<Result<Svc::Response, Svc::Error>, Error = Svc::Error>,
    {
        Pipeline {
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
    pub fn map<F, Res>(self, f: F) -> Pipeline<Req, Map<Svc, F, Req, Res>>
    where
        Self: Sized,
        F: Fn(Svc::Response) -> Res,
    {
        Pipeline {
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
    pub fn map_err<F, Err>(self, f: F) -> Pipeline<Req, MapErr<Svc, F, Err>>
    where
        Self: Sized,
        F: Fn(Svc::Error) -> Err,
    {
        Pipeline {
            service: MapErr::new(self.service, f),
            _t: PhantomData,
        }
    }

    /// Create service container
    pub fn container(self) -> Container<Svc> {
        Container::new(self.service)
    }
}

impl<Req, Svc> Clone for Pipeline<Req, Svc>
where
    Svc: Clone,
{
    fn clone(&self) -> Self {
        Pipeline {
            service: self.service.clone(),
            _t: PhantomData,
        }
    }
}

impl<Req, Svc: Service<Req>> Service<Req> for Pipeline<Req, Svc> {
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

/// Pipeline factory
pub struct PipelineFactory<Req, T, C = ()> {
    factory: T,
    _t: PhantomData<(Req, C)>,
}

impl<Req, T: ServiceFactory<Req, C>, C> PipelineFactory<Req, T, C> {
    /// Call another service after call to this one has resolved successfully.
    pub fn and_then<F, U>(self, factory: F) -> PipelineFactory<Req, AndThenFactory<T, U>, C>
    where
        Self: Sized,
        F: IntoServiceFactory<U, T::Response, C>,
        U: ServiceFactory<T::Response, C, Error = T::Error, InitError = T::InitError>,
    {
        PipelineFactory {
            factory: AndThenFactory::new(self.factory, factory.into_factory()),
            _t: PhantomData,
        }
    }

    /// Apply middleware to current service factory.
    ///
    /// Short version of `apply(middleware, pipeline_factory(...))`
    pub fn apply<U>(self, tr: U) -> PipelineFactory<Req, ApplyMiddleware<U, T, C>, C>
    where
        U: Middleware<T::Service>,
    {
        PipelineFactory {
            factory: ApplyMiddleware::new(tr, self.factory),
            _t: PhantomData,
        }
    }

    /// Create `NewService` to chain on a computation for when a call to the
    /// service finished, passing the result of the call to the next
    /// service `U`.
    ///
    /// Note that this function consumes the receiving pipeline and returns a
    /// wrapped version of it.
    pub fn then<F, U>(self, factory: F) -> PipelineFactory<Req, ThenFactory<T, U>, C>
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
        PipelineFactory {
            factory: ThenFactory::new(self.factory, factory.into_factory()),
            _t: PhantomData,
        }
    }

    /// Map this service's output to a different type, returning a new service
    /// of the resulting type.
    pub fn map<F, Res>(self, f: F) -> PipelineFactory<Req, MapFactory<T, F, Req, Res, C>, C>
    where
        Self: Sized,
        F: Fn(T::Response) -> Res + Clone,
    {
        PipelineFactory {
            factory: MapFactory::new(self.factory, f),
            _t: PhantomData,
        }
    }

    /// Map this service's error to a different error, returning a new service.
    pub fn map_err<F, E>(
        self,
        f: F,
    ) -> PipelineFactory<Req, MapErrFactory<T, Req, C, F, E>, C>
    where
        Self: Sized,
        F: Fn(T::Error) -> E + Clone,
    {
        PipelineFactory {
            factory: MapErrFactory::new(self.factory, f),
            _t: PhantomData,
        }
    }

    /// Map this factory's init error to a different error, returning a new service.
    pub fn map_init_err<F, E>(
        self,
        f: F,
    ) -> PipelineFactory<Req, MapInitErr<T, Req, C, F, E>, C>
    where
        Self: Sized,
        F: Fn(T::InitError) -> E + Clone,
    {
        PipelineFactory {
            factory: MapInitErr::new(self.factory, f),
            _t: PhantomData,
        }
    }
}

impl<Req, T, C> Clone for PipelineFactory<Req, T, C>
where
    T: Clone,
{
    fn clone(&self) -> Self {
        PipelineFactory {
            factory: self.factory.clone(),
            _t: PhantomData,
        }
    }
}

impl<Req, T: ServiceFactory<Req, C>, C> ServiceFactory<Req, C>
    for PipelineFactory<Req, T, C>
{
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
