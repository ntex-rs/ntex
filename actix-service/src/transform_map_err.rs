use std::marker::PhantomData;

use futures::{Async, Future, Poll};

use super::{NewTransform, Transform};

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
pub struct TransformMapErrNewTransform<T, S, C, F, E> {
    t: T,
    f: F,
    e: PhantomData<(S, C, E)>,
}

impl<T, S, C, F, E> TransformMapErrNewTransform<T, S, C, F, E> {
    /// Create new `MapErr` new service instance
    pub fn new(t: T, f: F) -> Self
    where
        T: NewTransform<S, C>,
        F: Fn(T::Error) -> E,
    {
        Self {
            t,
            f,
            e: PhantomData,
        }
    }
}

impl<T, S, C, F, E> Clone for TransformMapErrNewTransform<T, S, C, F, E>
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

impl<T, S, C, F, E> NewTransform<S, C> for TransformMapErrNewTransform<T, S, C, F, E>
where
    T: NewTransform<S, C>,
    F: Fn(T::Error) -> E + Clone,
{
    type Request = T::Request;
    type Response = T::Response;
    type Error = E;
    type Transform = TransformMapErr<T::Transform, S, F, E>;

    type InitError = T::InitError;
    type Future = TransformMapErrNewTransformFuture<T, S, C, F, E>;

    fn new_transform(&self, cfg: &C) -> Self::Future {
        TransformMapErrNewTransformFuture::new(self.t.new_transform(cfg), self.f.clone())
    }
}

pub struct TransformMapErrNewTransformFuture<T, S, C, F, E>
where
    T: NewTransform<S, C>,
    F: Fn(T::Error) -> E,
{
    fut: T::Future,
    f: F,
}

impl<T, S, C, F, E> TransformMapErrNewTransformFuture<T, S, C, F, E>
where
    T: NewTransform<S, C>,
    F: Fn(T::Error) -> E,
{
    fn new(fut: T::Future, f: F) -> Self {
        TransformMapErrNewTransformFuture { f, fut }
    }
}

impl<T, S, C, F, E> Future for TransformMapErrNewTransformFuture<T, S, C, F, E>
where
    T: NewTransform<S, C>,
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
