use std::rc::Rc;
use std::sync::Arc;

use futures::{Async, Future, IntoFuture, Poll};

use crate::transform_map_init_err::TransformMapInitErr;
use crate::{NewService, Service};

/// `Transform` service factory.
///
/// Transform factory creates service that wraps other services.
/// `Config` is a service factory configuration type.
pub trait Transform<S> {
    /// Requests handled by the service.
    type Request;

    /// Responses given by the service.
    type Response;

    /// Errors produced by the service.
    type Error;

    /// The `TransformService` value created by this factory
    type Transform: Service<
        Request = Self::Request,
        Response = Self::Response,
        Error = Self::Error,
    >;

    /// Errors produced while building a service.
    type InitError;

    /// The future response value.
    type Future: Future<Item = Self::Transform, Error = Self::InitError>;

    /// Create and return a new service value asynchronously.
    fn new_transform(&self, service: S) -> Self::Future;

    /// Map this service's factory init error to a different error,
    /// returning a new transform service factory.
    fn map_init_err<F, E>(self, f: F) -> TransformMapInitErr<Self, S, F, E>
    where
        Self: Sized,
        F: Fn(Self::InitError) -> E,
    {
        TransformMapInitErr::new(self, f)
    }
}

impl<T, S> Transform<S> for Rc<T>
where
    T: Transform<S>,
{
    type Request = T::Request;
    type Response = T::Response;
    type Error = T::Error;
    type InitError = T::InitError;
    type Transform = T::Transform;
    type Future = T::Future;

    fn new_transform(&self, service: S) -> T::Future {
        self.as_ref().new_transform(service)
    }
}

impl<T, S> Transform<S> for Arc<T>
where
    T: Transform<S>,
{
    type Request = T::Request;
    type Response = T::Response;
    type Error = T::Error;
    type InitError = T::InitError;
    type Transform = T::Transform;
    type Future = T::Future;

    fn new_transform(&self, service: S) -> T::Future {
        self.as_ref().new_transform(service)
    }
}

/// Trait for types that can be converted to a *transform service*
pub trait IntoTransform<T, S>
where
    T: Transform<S>,
{
    /// Convert to a `TransformService`
    fn into_transform(self) -> T;
}

impl<T, S> IntoTransform<T, S> for T
where
    T: Transform<S>,
{
    fn into_transform(self) -> T {
        self
    }
}

/// `Apply` transform new service
#[derive(Clone)]
pub struct ApplyTransform<T, A, C> {
    a: A,
    t: Rc<T>,
    _t: std::marker::PhantomData<C>,
}

impl<T, A, C> ApplyTransform<T, A, C>
where
    A: NewService<C>,
    T: Transform<A::Service, Error = A::Error, InitError = A::InitError>,
{
    /// Create new `ApplyNewService` new service instance
    pub fn new<F: IntoTransform<T, A::Service>>(t: F, a: A) -> Self {
        Self {
            a,
            t: Rc::new(t.into_transform()),
            _t: std::marker::PhantomData,
        }
    }
}

impl<T, A, C> NewService<C> for ApplyTransform<T, A, C>
where
    A: NewService<C>,
    T: Transform<A::Service, Error = A::Error, InitError = A::InitError>,
{
    type Request = T::Request;
    type Response = T::Response;
    type Error = T::Error;

    type Service = T::Transform;
    type InitError = T::InitError;
    type Future = ApplyTransformFuture<T, A, C>;

    fn new_service(&self, cfg: &C) -> Self::Future {
        ApplyTransformFuture {
            t_cell: self.t.clone(),
            fut_a: self.a.new_service(cfg).into_future(),
            fut_t: None,
        }
    }
}

pub struct ApplyTransformFuture<T, A, C>
where
    A: NewService<C>,
    T: Transform<A::Service, Error = A::Error, InitError = A::InitError>,
{
    fut_a: A::Future,
    fut_t: Option<<T::Future as IntoFuture>::Future>,
    t_cell: Rc<T>,
}

impl<T, A, C> Future for ApplyTransformFuture<T, A, C>
where
    A: NewService<C>,
    T: Transform<A::Service, Error = A::Error, InitError = A::InitError>,
{
    type Item = T::Transform;
    type Error = T::InitError;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        if self.fut_t.is_none() {
            if let Async::Ready(service) = self.fut_a.poll()? {
                self.fut_t = Some(self.t_cell.new_transform(service).into_future());
            }
        }
        if let Some(ref mut fut) = self.fut_t {
            fut.poll()
        } else {
            Ok(Async::NotReady)
        }
    }
}
