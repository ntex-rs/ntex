use std::task::{Context, Poll};

use crate::map::{Map, MapNewService};
use crate::map_err::{MapErr, MapErrNewService};
use crate::map_init_err::MapInitErr;
use crate::{IntoService, IntoServiceFactory, Service, ServiceFactory};

#[inline]
/// Convert object of type `U` to a service `T`
pub fn into_service<T, U>(service: U) -> ServiceMapper<T>
where
    U: IntoService<T>,
    T: Service,
{
    ServiceMapper {
        service: service.into_service(),
    }
}

pub fn into_factory<T, F>(factory: F) -> ServiceFactoryMapper<T>
where
    T: ServiceFactory,
    F: IntoServiceFactory<T>,
{
    ServiceFactoryMapper {
        factory: factory.into_factory(),
    }
}

pub struct ServiceMapper<T> {
    service: T,
}

pub struct ServiceFactoryMapper<T> {
    factory: T,
}

impl<T: Service> ServiceMapper<T> {
    /// Map this service's output to a different type, returning a new service
    /// of the resulting type.
    ///
    /// This function is similar to the `Option::map` or `Iterator::map` where
    /// it will change the type of the underlying service.
    ///
    /// Note that this function consumes the receiving service and returns a
    /// wrapped version of it, similar to the existing `map` methods in the
    /// standard library.
    pub fn map<F, R>(
        self,
        f: F,
    ) -> ServiceMapper<impl Service<Request = T::Request, Response = R, Error = T::Error>>
    where
        Self: Sized,
        F: FnMut(T::Response) -> R + Clone,
    {
        ServiceMapper {
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
    pub fn map_err<F, E>(
        self,
        f: F,
    ) -> ServiceMapper<impl Service<Request = T::Request, Response = T::Response, Error = E>>
    where
        Self: Sized,
        F: Fn(T::Error) -> E + Clone,
    {
        ServiceMapper {
            service: MapErr::new(self, f),
        }
    }
}

impl<T> Clone for ServiceMapper<T>
where
    T: Clone,
{
    fn clone(&self) -> Self {
        ServiceMapper {
            service: self.service.clone(),
        }
    }
}

impl<T: Service> Service for ServiceMapper<T> {
    type Request = T::Request;
    type Response = T::Response;
    type Error = T::Error;
    type Future = T::Future;

    #[inline]
    fn poll_ready(&mut self, ctx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(ctx)
    }

    #[inline]
    fn call(&mut self, req: T::Request) -> Self::Future {
        self.service.call(req)
    }
}

impl<T: ServiceFactory> ServiceFactoryMapper<T> {
    /// Map this service's output to a different type, returning a new service
    /// of the resulting type.
    pub fn map<F, R>(
        self,
        f: F,
    ) -> ServiceFactoryMapper<
        impl ServiceFactory<
            Config = T::Config,
            Request = T::Request,
            Response = R,
            Error = T::Error,
            InitError = T::InitError,
        >,
    >
    where
        Self: Sized,
        F: FnMut(T::Response) -> R + Clone,
    {
        ServiceFactoryMapper {
            factory: MapNewService::new(self.factory, f),
        }
    }

    /// Map this service's error to a different error, returning a new service.
    pub fn map_err<F, E>(
        self,
        f: F,
    ) -> ServiceFactoryMapper<
        impl ServiceFactory<
            Config = T::Config,
            Request = T::Request,
            Response = T::Response,
            Error = E,
            InitError = T::InitError,
        >,
    >
    where
        Self: Sized,
        F: Fn(T::Error) -> E + Clone,
    {
        ServiceFactoryMapper {
            factory: MapErrNewService::new(self.factory, f),
        }
    }

    /// Map this factory's init error to a different error, returning a new service.
    pub fn map_init_err<F, E>(
        self,
        f: F,
    ) -> ServiceFactoryMapper<
        impl ServiceFactory<
            Config = T::Config,
            Request = T::Request,
            Response = T::Response,
            Error = T::Error,
            InitError = E,
        >,
    >
    where
        Self: Sized,
        F: Fn(T::InitError) -> E + Clone,
    {
        ServiceFactoryMapper {
            factory: MapInitErr::new(self.factory, f),
        }
    }
}

impl<T> Clone for ServiceFactoryMapper<T>
where
    T: Clone,
{
    fn clone(&self) -> Self {
        ServiceFactoryMapper {
            factory: self.factory.clone(),
        }
    }
}

impl<T: ServiceFactory> ServiceFactory for ServiceFactoryMapper<T> {
    type Config = T::Config;
    type Request = T::Request;
    type Response = T::Response;
    type Error = T::Error;
    type Service = T::Service;
    type InitError = T::InitError;
    type Future = T::Future;

    #[inline]
    fn new_service(&self, cfg: &T::Config) -> Self::Future {
        self.factory.new_service(cfg)
    }
}
