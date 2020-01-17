use std::future::Future;
use std::task::{Context, Poll};

use crate::and_then::{AndThenService, AndThenServiceFactory};
use crate::and_then_apply_fn::{AndThenApplyFn, AndThenApplyFnFactory};
use crate::map::{Map, MapServiceFactory};
use crate::map_err::{MapErr, MapErrServiceFactory};
use crate::map_init_err::MapInitErr;
use crate::then::{ThenService, ThenServiceFactory};
use crate::{IntoService, IntoServiceFactory, Service, ServiceFactory};

/// Contruct new pipeline with one service in pipeline chain.
pub fn pipeline<F, T>(service: F) -> Pipeline<T>
where
    F: IntoService<T>,
    T: Service,
{
    Pipeline {
        service: service.into_service(),
    }
}

/// Contruct new pipeline factory with one service factory.
pub fn pipeline_factory<T, F>(factory: F) -> PipelineFactory<T>
where
    T: ServiceFactory,
    F: IntoServiceFactory<T>,
{
    PipelineFactory {
        factory: factory.into_factory(),
    }
}

/// Pipeline service - pipeline allows to compose multiple service into one service.
pub struct Pipeline<T> {
    service: T,
}

impl<T: Service> Pipeline<T> {
    /// Call another service after call to this one has resolved successfully.
    ///
    /// This function can be used to chain two services together and ensure that
    /// the second service isn't called until call to the fist service have
    /// finished. Result of the call to the first service is used as an
    /// input parameter for the second service's call.
    ///
    /// Note that this function consumes the receiving service and returns a
    /// wrapped version of it.
    pub fn and_then<F, U>(
        self,
        service: F,
    ) -> Pipeline<
        impl Service<Request = T::Request, Response = U::Response, Error = T::Error> + Clone,
    >
    where
        Self: Sized,
        F: IntoService<U>,
        U: Service<Request = T::Response, Error = T::Error>,
    {
        Pipeline {
            service: AndThenService::new(self.service, service.into_service()),
        }
    }

    /// Apply function to specified service and use it as a next service in
    /// chain.
    ///
    /// Short version of `pipeline_factory(...).and_then(apply_fn_factory(...))`
    pub fn and_then_apply_fn<U, I, F, Fut, Res, Err>(
        self,
        service: I,
        f: F,
    ) -> Pipeline<impl Service<Request = T::Request, Response = Res, Error = Err> + Clone>
    where
        Self: Sized,
        I: IntoService<U>,
        U: Service,
        F: FnMut(T::Response, &mut U) -> Fut,
        Fut: Future<Output = Result<Res, Err>>,
        Err: From<T::Error> + From<U::Error>,
    {
        Pipeline {
            service: AndThenApplyFn::new(self.service, service.into_service(), f),
        }
    }

    /// Chain on a computation for when a call to the service finished,
    /// passing the result of the call to the next service `U`.
    ///
    /// Note that this function consumes the receiving pipeline and returns a
    /// wrapped version of it.
    pub fn then<F, U>(
        self,
        service: F,
    ) -> Pipeline<
        impl Service<Request = T::Request, Response = U::Response, Error = T::Error> + Clone,
    >
    where
        Self: Sized,
        F: IntoService<U>,
        U: Service<Request = Result<T::Response, T::Error>, Error = T::Error>,
    {
        Pipeline {
            service: ThenService::new(self.service, service.into_service()),
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
    pub fn map<F, R>(self, f: F) -> Pipeline<Map<T, F, R>>
    where
        Self: Sized,
        F: FnMut(T::Response) -> R,
    {
        Pipeline {
            service: Map::new(self.service, f),
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
    pub fn map_err<F, E>(self, f: F) -> Pipeline<MapErr<T, F, E>>
    where
        Self: Sized,
        F: Fn(T::Error) -> E,
    {
        Pipeline {
            service: MapErr::new(self.service, f),
        }
    }
}

impl<T> Clone for Pipeline<T>
where
    T: Clone,
{
    fn clone(&self) -> Self {
        Pipeline {
            service: self.service.clone(),
        }
    }
}

impl<T: Service> Service for Pipeline<T> {
    type Request = T::Request;
    type Response = T::Response;
    type Error = T::Error;
    type Future = T::Future;

    #[inline]
    fn poll_ready(&mut self, ctx: &mut Context<'_>) -> Poll<Result<(), T::Error>> {
        self.service.poll_ready(ctx)
    }

    #[inline]
    fn call(&mut self, req: T::Request) -> Self::Future {
        self.service.call(req)
    }
}

/// Pipeline factory
pub struct PipelineFactory<T> {
    factory: T,
}

impl<T: ServiceFactory> PipelineFactory<T> {
    /// Call another service after call to this one has resolved successfully.
    pub fn and_then<F, U>(
        self,
        factory: F,
    ) -> PipelineFactory<
        impl ServiceFactory<
                Request = T::Request,
                Response = U::Response,
                Error = T::Error,
                Config = T::Config,
                InitError = T::InitError,
                Service = impl Service<
                    Request = T::Request,
                    Response = U::Response,
                    Error = T::Error,
                > + Clone,
            > + Clone,
    >
    where
        Self: Sized,
        T::Config: Clone,
        F: IntoServiceFactory<U>,
        U: ServiceFactory<
            Config = T::Config,
            Request = T::Response,
            Error = T::Error,
            InitError = T::InitError,
        >,
    {
        PipelineFactory {
            factory: AndThenServiceFactory::new(self.factory, factory.into_factory()),
        }
    }

    /// Apply function to specified service and use it as a next service in
    /// chain.
    ///
    /// Short version of `pipeline_factory(...).and_then(apply_fn_factory(...))`
    pub fn and_then_apply_fn<U, I, F, Fut, Res, Err>(
        self,
        factory: I,
        f: F,
    ) -> PipelineFactory<
        impl ServiceFactory<
                Request = T::Request,
                Response = Res,
                Error = Err,
                Config = T::Config,
                InitError = T::InitError,
                Service = impl Service<Request = T::Request, Response = Res, Error = Err> + Clone,
            > + Clone,
    >
    where
        Self: Sized,
        T::Config: Clone,
        I: IntoServiceFactory<U>,
        U: ServiceFactory<Config = T::Config, InitError = T::InitError>,
        F: FnMut(T::Response, &mut U::Service) -> Fut + Clone,
        Fut: Future<Output = Result<Res, Err>>,
        Err: From<T::Error> + From<U::Error>,
    {
        PipelineFactory {
            factory: AndThenApplyFnFactory::new(self.factory, factory.into_factory(), f),
        }
    }

    /// Create `NewService` to chain on a computation for when a call to the
    /// service finished, passing the result of the call to the next
    /// service `U`.
    ///
    /// Note that this function consumes the receiving pipeline and returns a
    /// wrapped version of it.
    pub fn then<F, U>(
        self,
        factory: F,
    ) -> PipelineFactory<
        impl ServiceFactory<
                Request = T::Request,
                Response = U::Response,
                Error = T::Error,
                Config = T::Config,
                InitError = T::InitError,
                Service = impl Service<
                    Request = T::Request,
                    Response = U::Response,
                    Error = T::Error,
                > + Clone,
            > + Clone,
    >
    where
        Self: Sized,
        T::Config: Clone,
        F: IntoServiceFactory<U>,
        U: ServiceFactory<
            Config = T::Config,
            Request = Result<T::Response, T::Error>,
            Error = T::Error,
            InitError = T::InitError,
        >,
    {
        PipelineFactory {
            factory: ThenServiceFactory::new(self.factory, factory.into_factory()),
        }
    }

    /// Map this service's output to a different type, returning a new service
    /// of the resulting type.
    pub fn map<F, R>(self, f: F) -> PipelineFactory<MapServiceFactory<T, F, R>>
    where
        Self: Sized,
        F: FnMut(T::Response) -> R + Clone,
    {
        PipelineFactory {
            factory: MapServiceFactory::new(self.factory, f),
        }
    }

    /// Map this service's error to a different error, returning a new service.
    pub fn map_err<F, E>(self, f: F) -> PipelineFactory<MapErrServiceFactory<T, F, E>>
    where
        Self: Sized,
        F: Fn(T::Error) -> E + Clone,
    {
        PipelineFactory {
            factory: MapErrServiceFactory::new(self.factory, f),
        }
    }

    /// Map this factory's init error to a different error, returning a new service.
    pub fn map_init_err<F, E>(self, f: F) -> PipelineFactory<MapInitErr<T, F, E>>
    where
        Self: Sized,
        F: Fn(T::InitError) -> E + Clone,
    {
        PipelineFactory {
            factory: MapInitErr::new(self.factory, f),
        }
    }
}

impl<T> Clone for PipelineFactory<T>
where
    T: Clone,
{
    fn clone(&self) -> Self {
        PipelineFactory {
            factory: self.factory.clone(),
        }
    }
}

impl<T: ServiceFactory> ServiceFactory for PipelineFactory<T> {
    type Config = T::Config;
    type Request = T::Request;
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
