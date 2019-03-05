use std::marker::PhantomData;

use futures::{Future, IntoFuture, Poll};

use super::NewService;

/// `MapInitErr` service combinator
pub struct MapInitErr<A, F, E, C> {
    a: A,
    f: F,
    e: PhantomData<(E, C)>,
}

impl<A, F, E, C> MapInitErr<A, F, E, C> {
    /// Create new `MapInitErr` combinator
    pub fn new(a: A, f: F) -> Self
    where
        A: NewService<C>,
        F: Fn(A::InitError) -> E,
    {
        Self {
            a,
            f,
            e: PhantomData,
        }
    }
}

impl<A, F, E, C> Clone for MapInitErr<A, F, E, C>
where
    A: Clone,
    F: Clone,
{
    fn clone(&self) -> Self {
        Self {
            a: self.a.clone(),
            f: self.f.clone(),
            e: PhantomData,
        }
    }
}

impl<A, F, E, C> NewService<C> for MapInitErr<A, F, E, C>
where
    A: NewService<C>,
    F: Fn(A::InitError) -> E + Clone,
{
    type Request = A::Request;
    type Response = A::Response;
    type Error = A::Error;
    type Service = A::Service;

    type InitError = E;
    type Future = MapInitErrFuture<A, F, E, C>;

    fn new_service(&self, cfg: &C) -> Self::Future {
        MapInitErrFuture::new(self.a.new_service(cfg).into_future(), self.f.clone())
    }
}

pub struct MapInitErrFuture<A, F, E, C>
where
    A: NewService<C>,
    F: Fn(A::InitError) -> E,
{
    f: F,
    fut: <A::Future as IntoFuture>::Future,
}

impl<A, F, E, C> MapInitErrFuture<A, F, E, C>
where
    A: NewService<C>,
    F: Fn(A::InitError) -> E,
{
    fn new(fut: <A::Future as IntoFuture>::Future, f: F) -> Self {
        MapInitErrFuture { f, fut }
    }
}

impl<A, F, E, C> Future for MapInitErrFuture<A, F, E, C>
where
    A: NewService<C>,
    F: Fn(A::InitError) -> E,
{
    type Item = A::Service;
    type Error = E;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.fut.poll().map_err(&self.f)
    }
}
