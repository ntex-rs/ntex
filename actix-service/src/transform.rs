use std::rc::Rc;
use std::sync::Arc;

use futures::{Future, Poll};

use crate::transform_map_err::{TransformMapErr, TransformMapErrNewTransform};
use crate::transform_map_init_err::TransformMapInitErr;
use crate::Service;

/// An asynchronous function for transforming service call result.
pub trait Transform<Service> {
    /// Requests handled by the service.
    type Request;

    /// Responses given by the service.
    type Response;

    /// Errors produced by the service.
    type Error;

    /// The future response value.
    type Future: Future<Item = Self::Response, Error = Self::Error>;

    /// Returns `Ready` when the service is able to process requests.
    ///
    /// This method is similar to `Service::poll_ready` method.
    fn poll_ready(&mut self) -> Poll<(), Self::Error>;

    /// Process the request and apply it to provided service,
    /// return the response asynchronously.
    fn call(&mut self, request: Self::Request, service: &mut Service) -> Self::Future;

    /// Map this transform's error to a different error, returning a new transform.
    ///
    /// This function is similar to the `Result::map_err` where it will change
    /// the error type of the underlying transform. This is useful for example to
    /// ensure that services and transforms have the same error type.
    ///
    /// Note that this function consumes the receiving transform and returns a
    /// wrapped version of it.
    fn map_err<F, E>(self, f: F) -> TransformMapErr<Self, Service, F, E>
    where
        Self: Sized,
        F: Fn(Self::Error) -> E,
    {
        TransformMapErr::new(self, f)
    }
}

/// `Transform` service factory
///
/// `Config` is a service factory configuration type.
pub trait NewTransform<Service, Config = ()> {
    /// Requests handled by the service.
    type Request;

    /// Responses given by the service.
    type Response;

    /// Errors produced by the service.
    type Error;

    /// The `TransformService` value created by this factory
    type Transform: Transform<
        Service,
        Request = Self::Request,
        Response = Self::Response,
        Error = Self::Error,
    >;

    /// Errors produced while building a service.
    type InitError;

    /// The future response value.
    type Future: Future<Item = Self::Transform, Error = Self::InitError>;

    /// Create and return a new service value asynchronously.
    fn new_transform(&self, cfg: &Config) -> Self::Future;

    /// Map this transforms's output to a different type, returning a new transform
    /// of the resulting type.
    fn map_err<F, E>(self, f: F) -> TransformMapErrNewTransform<Self, Service, Config, F, E>
    where
        Self: Sized,
        F: Fn(Self::Error) -> E,
    {
        TransformMapErrNewTransform::new(self, f)
    }

    /// Map this service's factory init error to a different error,
    /// returning a new transform service factory.
    fn map_init_err<F, E>(self, f: F) -> TransformMapInitErr<Self, Service, Config, F, E>
    where
        Self: Sized,
        F: Fn(Self::InitError) -> E,
    {
        TransformMapInitErr::new(self, f)
    }
}

impl<'a, T, S> Transform<S> for &'a mut T
where
    T: Transform<S> + 'a,
    S: Service<Error = T::Error>,
{
    type Request = T::Request;
    type Response = T::Response;
    type Error = T::Error;
    type Future = T::Future;

    fn poll_ready(&mut self) -> Poll<(), T::Error> {
        (**self).poll_ready()
    }

    fn call(&mut self, request: Self::Request, service: &mut S) -> T::Future {
        (**self).call(request, service)
    }
}

impl<T, S> Transform<S> for Box<T>
where
    T: Transform<S> + ?Sized,
    S: Service<Error = T::Error>,
{
    type Request = T::Request;
    type Response = T::Response;
    type Error = T::Error;
    type Future = T::Future;

    fn poll_ready(&mut self) -> Poll<(), S::Error> {
        (**self).poll_ready()
    }

    fn call(&mut self, request: Self::Request, service: &mut S) -> T::Future {
        (**self).call(request, service)
    }
}

impl<S, C, T> NewTransform<S, C> for Rc<T>
where
    T: NewTransform<S, C>,
{
    type Request = T::Request;
    type Response = T::Response;
    type Error = T::Error;
    type Transform = T::Transform;
    type InitError = T::InitError;
    type Future = T::Future;

    fn new_transform(&self, cfg: &C) -> T::Future {
        self.as_ref().new_transform(cfg)
    }
}

impl<S, C, T> NewTransform<S, C> for Arc<T>
where
    T: NewTransform<S, C>,
{
    type Request = T::Request;
    type Response = T::Response;
    type Error = T::Error;
    type Transform = T::Transform;
    type InitError = T::InitError;
    type Future = T::Future;

    fn new_transform(&self, cfg: &C) -> T::Future {
        self.as_ref().new_transform(cfg)
    }
}

/// Trait for types that can be converted to a `TransformService`
pub trait IntoTransform<T, S>
where
    T: Transform<S>,
{
    /// Convert to a `TransformService`
    fn into_transform(self) -> T;
}

/// Trait for types that can be converted to a TransfromNewService
pub trait IntoNewTransform<T, S, C = ()>
where
    T: NewTransform<S, C>,
{
    /// Convert to an `TranformNewService`
    fn into_new_transform(self) -> T;
}

impl<T, S> IntoTransform<T, S> for T
where
    T: Transform<S>,
{
    fn into_transform(self) -> T {
        self
    }
}

impl<T, S, C> IntoNewTransform<T, S, C> for T
where
    T: NewTransform<S, C>,
{
    fn into_new_transform(self) -> T {
        self
    }
}
