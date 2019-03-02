use std::marker::PhantomData;

use futures::{Future, Poll};

use super::NewTransform;

/// NewTransform for the `map_init_err` combinator, changing the type of a new
/// transform's error.
///
/// This is created by the `NewTransform::map_init_err` method.
pub struct TransformMapInitErr<T, S, C, F, E> {
    t: T,
    f: F,
    e: PhantomData<(S, C, E)>,
}

impl<T, S, C, F, E> TransformMapInitErr<T, S, C, F, E> {
    /// Create new `MapInitErr` new transform instance
    pub fn new(t: T, f: F) -> Self
    where
        T: NewTransform<S, C>,
        F: Fn(T::InitError) -> E,
    {
        Self {
            t,
            f,
            e: PhantomData,
        }
    }
}

impl<T, S, C, F, E> Clone for TransformMapInitErr<T, S, C, F, E>
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

impl<T, S, C, F, E> NewTransform<S, C> for TransformMapInitErr<T, S, C, F, E>
where
    T: NewTransform<S, C>,
    F: Fn(T::InitError) -> E + Clone,
{
    type Request = T::Request;
    type Response = T::Response;
    type Error = T::Error;
    type Transform = T::Transform;

    type InitError = E;
    type Future = TransformMapInitErrFuture<T, S, C, F, E>;

    fn new_transform(&self, cfg: &C) -> Self::Future {
        TransformMapInitErrFuture::new(self.t.new_transform(cfg), self.f.clone())
    }
}

pub struct TransformMapInitErrFuture<T, S, C, F, E>
where
    T: NewTransform<S, C>,
    F: Fn(T::InitError) -> E,
{
    fut: T::Future,
    f: F,
}

impl<T, S, C, F, E> TransformMapInitErrFuture<T, S, C, F, E>
where
    T: NewTransform<S, C>,
    F: Fn(T::InitError) -> E,
{
    fn new(fut: T::Future, f: F) -> Self {
        TransformMapInitErrFuture { f, fut }
    }
}

impl<T, S, C, F, E> Future for TransformMapInitErrFuture<T, S, C, F, E>
where
    T: NewTransform<S, C>,
    F: Fn(T::InitError) -> E + Clone,
{
    type Item = T::Transform;
    type Error = E;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.fut.poll().map_err(|e| (self.f)(e))
    }
}
