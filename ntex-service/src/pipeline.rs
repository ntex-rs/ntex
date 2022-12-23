use std::{marker::PhantomData, task::Context, task::Poll};

use crate::and_then::{AndThen, AndThenFactory};
use crate::map::{Map, MapFactory};
use crate::map_err::{MapErr, MapErrFactory};
use crate::map_init_err::MapInitErr;
use crate::then::{Then, ThenFactory};
use crate::transform::{ApplyMiddleware, Middleware};
use crate::{IntoService, IntoServiceFactory, Service, ServiceFactory};

/// Constructs new pipeline with one service in pipeline chain.
pub fn pipeline<T, R, F>(service: F) -> Pipeline<T, R>
where
    T: Service<R>,
    F: IntoService<T, R>,
{
    Pipeline {
        service: service.into_service(),
        _t: PhantomData,
    }
}

/// Constructs new pipeline factory with one service factory.
pub fn pipeline_factory<T, R, C, F>(factory: F) -> PipelineFactory<T, R, C>
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
pub struct Pipeline<T, R> {
    service: T,
    _t: PhantomData<R>,
}

impl<T: Service<R>, R> Pipeline<T, R> {
    /// Call another service after call to this one has resolved successfully.
    ///
    /// This function can be used to chain two services together and ensure that
    /// the second service isn't called until call to the fist service have
    /// finished. Result of the call to the first service is used as an
    /// input parameter for the second service's call.
    ///
    /// Note that this function consumes the receiving service and returns a
    /// wrapped version of it.
    pub fn and_then<F, U>(self, service: F) -> Pipeline<AndThen<T, U>, R>
    where
        Self: Sized,
        F: IntoService<U, T::Response>,
        U: Service<T::Response, Error = T::Error>,
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
    pub fn then<F, U>(self, service: F) -> Pipeline<Then<T, U>, R>
    where
        Self: Sized,
        F: IntoService<U, Result<T::Response, T::Error>>,
        U: Service<Result<T::Response, T::Error>, Error = T::Error>,
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
    pub fn map<F, Res>(self, f: F) -> Pipeline<Map<T, F, R, Res>, R>
    where
        Self: Sized,
        F: Fn(T::Response) -> Res,
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
    pub fn map_err<F, E>(self, f: F) -> Pipeline<MapErr<T, R, F, E>, R>
    where
        Self: Sized,
        F: Fn(T::Error) -> E,
    {
        Pipeline {
            service: MapErr::new(self.service, f),
            _t: PhantomData,
        }
    }
}

impl<T, R> Clone for Pipeline<T, R>
where
    T: Clone,
{
    fn clone(&self) -> Self {
        Pipeline {
            service: self.service.clone(),
            _t: PhantomData,
        }
    }
}

impl<T: Service<R>, R> Service<R> for Pipeline<T, R> {
    type Response = T::Response;
    type Error = T::Error;
    type Future<'f> = T::Future<'f> where Self: 'f, R: 'f;

    #[inline]
    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), T::Error>> {
        self.service.poll_ready(cx)
    }

    #[inline]
    fn poll_shutdown(&self, cx: &mut Context<'_>, is_error: bool) -> Poll<()> {
        self.service.poll_shutdown(cx, is_error)
    }

    #[inline]
    fn call(&self, req: R) -> Self::Future<'_> {
        self.service.call(req)
    }
}

/// Pipeline factory
pub struct PipelineFactory<T, R, C = ()> {
    factory: T,
    _t: PhantomData<(R, C)>,
}

impl<T: ServiceFactory<R, C>, R, C> PipelineFactory<T, R, C> {
    /// Call another service after call to this one has resolved successfully.
    pub fn and_then<F, U>(self, factory: F) -> PipelineFactory<AndThenFactory<T, U>, R, C>
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
    pub fn apply<U>(self, tr: U) -> PipelineFactory<ApplyMiddleware<U, T, C>, R, C>
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
    pub fn then<F, U>(self, factory: F) -> PipelineFactory<ThenFactory<T, U>, R, C>
    where
        Self: Sized,
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
    pub fn map<F, Res>(self, f: F) -> PipelineFactory<MapFactory<T, F, R, Res, C>, R, C>
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
    pub fn map_err<F, E>(self, f: F) -> PipelineFactory<MapErrFactory<T, R, C, F, E>, R, C>
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
    ) -> PipelineFactory<MapInitErr<T, R, C, F, E>, R, C>
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

impl<T, R, C> Clone for PipelineFactory<T, R, C>
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

impl<T: ServiceFactory<R, C>, R, C> ServiceFactory<R, C> for PipelineFactory<T, R, C> {
    type Response = T::Response;
    type Error = T::Error;
    type Service = T::Service;
    type InitError = T::InitError;
    type Future<'f> = T::Future<'f> where Self: 'f, C: 'f;

    #[inline]
    fn create<'a>(&'a self, cfg: &'a C) -> Self::Future<'a> {
        self.factory.create(cfg)
    }
}
