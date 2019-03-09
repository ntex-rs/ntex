use std::marker::PhantomData;

use futures::{Future, Poll};

use super::Transform;

/// NewTransform for the `map_init_err` combinator, changing the type of a new
/// transform's error.
///
/// This is created by the `NewTransform::map_init_err` method.
pub struct TransformMapInitErr<T, S, F, E> {
    t: T,
    f: F,
    e: PhantomData<(S, E)>,
}

impl<T, S, F, E> TransformMapInitErr<T, S, F, E> {
    /// Create new `MapInitErr` new transform instance
    pub fn new(t: T, f: F) -> Self
    where
        T: Transform<S>,
        F: Fn(T::InitError) -> E,
    {
        Self {
            t,
            f,
            e: PhantomData,
        }
    }
}

impl<T, S, F, E> Clone for TransformMapInitErr<T, S, F, E>
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

impl<T, S, F, E> Transform<S> for TransformMapInitErr<T, S, F, E>
where
    T: Transform<S>,
    F: Fn(T::InitError) -> E + Clone,
{
    type Request = T::Request;
    type Response = T::Response;
    type Error = T::Error;
    type Transform = T::Transform;

    type InitError = E;
    type Future = TransformMapInitErrFuture<T, S, F, E>;

    fn new_transform(&self, service: S) -> Self::Future {
        TransformMapInitErrFuture::new(self.t.new_transform(service), self.f.clone())
    }
}

pub struct TransformMapInitErrFuture<T, S, F, E>
where
    T: Transform<S>,
    F: Fn(T::InitError) -> E,
{
    fut: T::Future,
    f: F,
}

impl<T, S, F, E> TransformMapInitErrFuture<T, S, F, E>
where
    T: Transform<S>,
    F: Fn(T::InitError) -> E,
{
    fn new(fut: T::Future, f: F) -> Self {
        TransformMapInitErrFuture { f, fut }
    }
}

impl<T, S, F, E> Future for TransformMapInitErrFuture<T, S, F, E>
where
    T: Transform<S>,
    F: Fn(T::InitError) -> E + Clone,
{
    type Item = T::Transform;
    type Error = E;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.fut.poll().map_err(&self.f)
    }
}
