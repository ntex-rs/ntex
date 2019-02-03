use std::marker::PhantomData;

use futures::{Async, Future, Poll};

use super::Service;

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
pub trait NewTransform<Service> {
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
    fn new_transform(&self) -> Self::Future;

    /// Map this transforms's output to a different type, returning a new transform
    /// of the resulting type.
    fn map_err<F, E>(self, f: F) -> TransformMapErrNewTransform<Self, Service, F, E>
    where
        Self: Sized,
        F: Fn(Self::Error) -> E,
    {
        TransformMapErrNewTransform::new(self, f)
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

/// Trait for types that can be converted to a `TransformService`
pub trait IntoTransform<T, S>
where
    T: Transform<S>,
{
    /// Convert to a `TransformService`
    fn into_transform(self) -> T;
}

/// Trait for types that can be converted to a TransfromNewService
pub trait IntoNewTransform<T, S>
where
    T: NewTransform<S>,
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

impl<T, S> IntoNewTransform<T, S> for T
where
    T: NewTransform<S>,
{
    fn into_new_transform(self) -> T {
        self
    }
}

/// Service for the `map_err` combinator, changing the type of a transform's
/// error.
///
/// This is created by the `Transform::map_err` method.
pub struct TransformMapErr<T, S, F, E> {
    transform: T,
    f: F,
    _t: PhantomData<(S, E)>,
}

impl<T, S, F, E> TransformMapErr<T, S, F, E> {
    /// Create new `MapErr` combinator
    pub fn new(transform: T, f: F) -> Self
    where
        T: Transform<S>,
        F: Fn(T::Error) -> E,
    {
        Self {
            transform,
            f,
            _t: PhantomData,
        }
    }
}

impl<T, S, F, E> Clone for TransformMapErr<T, S, F, E>
where
    T: Clone,
    F: Clone,
{
    fn clone(&self) -> Self {
        TransformMapErr {
            transform: self.transform.clone(),
            f: self.f.clone(),
            _t: PhantomData,
        }
    }
}

impl<T, S, F, E> Transform<S> for TransformMapErr<T, S, F, E>
where
    T: Transform<S>,
    F: Fn(T::Error) -> E + Clone,
{
    type Request = T::Request;
    type Response = T::Response;
    type Error = E;
    type Future = TransformMapErrFuture<T, S, F, E>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.transform.poll_ready().map_err(&self.f)
    }

    fn call(&mut self, req: T::Request, service: &mut S) -> Self::Future {
        TransformMapErrFuture::new(self.transform.call(req, service), self.f.clone())
    }
}

pub struct TransformMapErrFuture<T, S, F, E>
where
    T: Transform<S>,
    F: Fn(T::Error) -> E,
{
    f: F,
    fut: T::Future,
}

impl<T, S, F, E> TransformMapErrFuture<T, S, F, E>
where
    T: Transform<S>,
    F: Fn(T::Error) -> E,
{
    fn new(fut: T::Future, f: F) -> Self {
        TransformMapErrFuture { f, fut }
    }
}

impl<T, S, F, E> Future for TransformMapErrFuture<T, S, F, E>
where
    T: Transform<S>,
    F: Fn(T::Error) -> E,
{
    type Item = T::Response;
    type Error = E;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.fut.poll().map_err(&self.f)
    }
}

/// NewTransform for the `map_err` combinator, changing the type of a new
/// transform's error.
///
/// This is created by the `NewTransform::map_err` method.
pub struct TransformMapErrNewTransform<T, S, F, E> {
    t: T,
    f: F,
    e: PhantomData<(S, E)>,
}

impl<T, S, F, E> TransformMapErrNewTransform<T, S, F, E> {
    /// Create new `MapErr` new service instance
    pub fn new(t: T, f: F) -> Self
    where
        T: NewTransform<S>,
        F: Fn(T::Error) -> E,
    {
        Self {
            t,
            f,
            e: PhantomData,
        }
    }
}

impl<T, S, F, E> Clone for TransformMapErrNewTransform<T, S, F, E>
where
    T: Clone,
    F: Clone,
{
    fn clone(&self) -> Self {
        Self {
            t: self.t.clone(),
            f: self.f.clone(),
            e: PhantomData,
        }
    }
}

impl<T, S, F, E> NewTransform<S> for TransformMapErrNewTransform<T, S, F, E>
where
    T: NewTransform<S>,
    F: Fn(T::Error) -> E + Clone,
{
    type Request = T::Request;
    type Response = T::Response;
    type Error = E;
    type Transform = TransformMapErr<T::Transform, S, F, E>;

    type InitError = T::InitError;
    type Future = TransformMapErrNewTransformFuture<T, S, F, E>;

    fn new_transform(&self) -> Self::Future {
        TransformMapErrNewTransformFuture::new(self.t.new_transform(), self.f.clone())
    }
}

pub struct TransformMapErrNewTransformFuture<T, S, F, E>
where
    T: NewTransform<S>,
    F: Fn(T::Error) -> E,
{
    fut: T::Future,
    f: F,
}

impl<T, S, F, E> TransformMapErrNewTransformFuture<T, S, F, E>
where
    T: NewTransform<S>,
    F: Fn(T::Error) -> E,
{
    fn new(fut: T::Future, f: F) -> Self {
        TransformMapErrNewTransformFuture { f, fut }
    }
}

impl<T, S, F, E> Future for TransformMapErrNewTransformFuture<T, S, F, E>
where
    T: NewTransform<S>,
    F: Fn(T::Error) -> E + Clone,
{
    type Item = TransformMapErr<T::Transform, S, F, E>;
    type Error = T::InitError;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        if let Async::Ready(tr) = self.fut.poll()? {
            Ok(Async::Ready(TransformMapErr::new(tr, self.f.clone())))
        } else {
            Ok(Async::NotReady)
        }
    }
}
