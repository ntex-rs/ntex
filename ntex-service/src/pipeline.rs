use std::{marker::PhantomData, task::Context, task::Poll};

use crate::and_then::{AndThen, AndThenFactory};
// use crate::and_then_apply_fn::{AndThenApplyFn, AndThenApplyFnFactory};
use crate::map::{Map, MapServiceFactory};
use crate::map_err::{MapErr, MapErrServiceFactory};
use crate::map_init_err::MapInitErr;
use crate::then::{Then, ThenFactory};
use crate::transform::{ApplyTransform, Transform};
use crate::{IntoService, IntoServiceFactory, Service, ServiceFactory};

/// Contruct new pipeline with one service in pipeline chain.
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

/// Contruct new pipeline factory with one service factory.
pub fn pipeline_factory<T, R, F>(factory: F) -> PipelineFactory<T, R>
where
    T: ServiceFactory<R>,
    F: IntoServiceFactory<T, R>,
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
    pub fn and_then<F, U>(self, service: F) -> Pipeline<AndThen<T, U, R>, R>
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

    // /// Apply function to specified service and use it as a next service in
    // /// chain.
    // ///
    // /// Short version of `pipeline_factory(...).and_then(apply_fn_factory(...))`
    // pub fn and_then_apply_fn<U, I, F, Fut, Res, Err>(
    //     self,
    //     service: I,
    //     f: F,
    // ) -> Pipeline<impl Service<Request = T::Request, Response = Res, Error = Err> + Clone>
    // where
    //     Self: Sized,
    //     I: IntoService<U>,
    //     U: Service,
    //     F: Fn(T::Response, &U) -> Fut,
    //     Fut: Future<Output = Result<Res, Err>>,
    //     Err: From<T::Error> + From<U::Error>,
    // {
    //     Pipeline {
    //         service: AndThenApplyFn::new(self.service, service.into_service(), f),
    //     }
    // }

    /// Chain on a computation for when a call to the service finished,
    /// passing the result of the call to the next service `U`.
    ///
    /// Note that this function consumes the receiving pipeline and returns a
    /// wrapped version of it.
    pub fn then<F, U>(self, service: F) -> Pipeline<Then<T, U, R>, R>
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
        F: FnMut(T::Response) -> Res,
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
    type Future = T::Future;

    #[inline]
    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), T::Error>> {
        self.service.poll_ready(cx)
    }

    #[inline]
    fn poll_shutdown(&self, cx: &mut Context<'_>, is_error: bool) -> Poll<()> {
        self.service.poll_shutdown(cx, is_error)
    }

    #[inline]
    fn call(&self, req: R) -> Self::Future {
        self.service.call(req)
    }
}

/// Pipeline factory
pub struct PipelineFactory<T, R> {
    factory: T,
    _t: PhantomData<R>,
}

impl<T: ServiceFactory<R>, R> PipelineFactory<T, R> {
    /// Call another service after call to this one has resolved successfully.
    pub fn and_then<F, U>(
        self,
        factory: F,
    ) -> PipelineFactory<AndThenFactory<T, U, R>, R>
    where
        Self: Sized,
        T::Config: Clone,
        F: IntoServiceFactory<U, T::Response>,
        U: ServiceFactory<
            T::Response,
            Config = T::Config,
            Error = T::Error,
            InitError = T::InitError,
        >,
    {
        PipelineFactory {
            factory: AndThenFactory::new(self.factory, factory.into_factory()),
            _t: PhantomData,
        }
    }

    // /// Apply function to specified service and use it as a next service in
    // /// chain.
    // ///
    // /// Short version of `pipeline_factory(...).and_then(apply_fn_factory(...))`
    // pub fn and_then_apply_fn<U, I, F, Fut, Res, Err>(
    //     self,
    //     factory: I,
    //     f: F,
    // ) -> PipelineFactory<
    //     impl ServiceFactory<
    //             Request = T::Request,
    //             Response = Res,
    //             Error = Err,
    //             Config = T::Config,
    //             InitError = T::InitError,
    //             Service = impl Service<Request = T::Request, Response = Res, Error = Err>
    //                           + Clone,
    //         > + Clone,
    // >
    // where
    //     Self: Sized,
    //     T::Config: Clone,
    //     I: IntoServiceFactory<U>,
    //     U: ServiceFactory<Config = T::Config, InitError = T::InitError>,
    //     F: Fn(T::Response, &U::Service) -> Fut + Clone,
    //     Fut: Future<Output = Result<Res, Err>>,
    //     Err: From<T::Error> + From<U::Error>,
    // {
    //     PipelineFactory {
    //         factory: AndThenApplyFnFactory::new(self.factory, factory.into_factory(), f),
    //     }
    // }

    /// Apply transform to current service factory.
    ///
    /// Short version of `apply(transform, pipeline_factory(...))`
    pub fn apply<U>(self, tr: U) -> PipelineFactory<ApplyTransform<U, T, R>, R>
    where
        U: Transform<T::Service>,
    {
        PipelineFactory {
            factory: ApplyTransform::new(tr, self.factory),
            _t: PhantomData,
        }
    }

    /// Create `NewService` to chain on a computation for when a call to the
    /// service finished, passing the result of the call to the next
    /// service `U`.
    ///
    /// Note that this function consumes the receiving pipeline and returns a
    /// wrapped version of it.
    pub fn then<F, U>(self, factory: F) -> PipelineFactory<ThenFactory<T, U, R>, R>
    where
        Self: Sized,
        T::Config: Clone,
        F: IntoServiceFactory<U, Result<T::Response, T::Error>>,
        U: ServiceFactory<
            Result<T::Response, T::Error>,
            Config = T::Config,
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
    pub fn map<F, Res>(self, f: F) -> PipelineFactory<MapServiceFactory<T, F, R, Res>, R>
    where
        Self: Sized,
        F: FnMut(T::Response) -> Res + Clone,
    {
        PipelineFactory {
            factory: MapServiceFactory::new(self.factory, f),
            _t: PhantomData,
        }
    }

    /// Map this service's error to a different error, returning a new service.
    pub fn map_err<F, E>(
        self,
        f: F,
    ) -> PipelineFactory<MapErrServiceFactory<T, R, F, E>, R>
    where
        Self: Sized,
        F: Fn(T::Error) -> E + Clone,
    {
        PipelineFactory {
            factory: MapErrServiceFactory::new(self.factory, f),
            _t: PhantomData,
        }
    }

    /// Map this factory's init error to a different error, returning a new service.
    pub fn map_init_err<F, E>(self, f: F) -> PipelineFactory<MapInitErr<T, R, F, E>, R>
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

impl<T, R> Clone for PipelineFactory<T, R>
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

impl<T: ServiceFactory<R>, R> ServiceFactory<R> for PipelineFactory<T, R> {
    type Config = T::Config;
    type Response = T::Response;
    type Error = T::Error;
    type Service = T::Service;
    type InitError = T::InitError;
    type Future = T::Future;

    #[inline]
    fn new_service(&self, cfg: T::Config) -> Self::Future {
        self.factory.new_service(cfg)
    }
}
