//! Contains `Either` service and related types and functions.
use actix_service::{NewService, Service};
use futures::{future, try_ready, Async, Future, IntoFuture, Poll};

/// Combine two different service types into a single type.
///
/// Both services must be of the same request, response, and error types.
/// `EitherService` is useful for handling conditional branching in service
/// middleware to different inner service types.
pub enum EitherService<A, B> {
    A(A),
    B(B),
}

impl<A: Clone, B: Clone> Clone for EitherService<A, B> {
    fn clone(&self) -> Self {
        match self {
            EitherService::A(srv) => EitherService::A(srv.clone()),
            EitherService::B(srv) => EitherService::B(srv.clone()),
        }
    }
}

impl<A, B, R> Service<R> for EitherService<A, B>
where
    A: Service<R>,
    B: Service<R, Response = A::Response, Error = A::Error>,
{
    type Response = A::Response;
    type Error = A::Error;
    type Future = future::Either<A::Future, B::Future>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        match self {
            EitherService::A(ref mut inner) => inner.poll_ready(),
            EitherService::B(ref mut inner) => inner.poll_ready(),
        }
    }

    fn call(&mut self, req: R) -> Self::Future {
        match self {
            EitherService::A(ref mut inner) => future::Either::A(inner.call(req)),
            EitherService::B(ref mut inner) => future::Either::B(inner.call(req)),
        }
    }
}

/// Combine two different new service types into a single type.
pub enum Either<A, B> {
    A(A),
    B(B),
}

impl<A, B> Either<A, B> {
    pub fn new_a<R, C>(srv: A) -> Self
    where
        A: NewService<R, C>,
        B: NewService<R, C, Response = A::Response, Error = A::Error, InitError = A::InitError>,
    {
        Either::A(srv)
    }

    pub fn new_b<R, C>(srv: B) -> Self
    where
        A: NewService<R, C>,
        B: NewService<R, C, Response = A::Response, Error = A::Error, InitError = A::InitError>,
    {
        Either::B(srv)
    }
}

impl<A, B, R, C> NewService<R, C> for Either<A, B>
where
    A: NewService<R, C>,
    B: NewService<R, C, Response = A::Response, Error = A::Error, InitError = A::InitError>,
{
    type Response = A::Response;
    type Error = A::Error;
    type InitError = A::InitError;
    type Service = EitherService<A::Service, B::Service>;
    type Future = EitherNewService<A, B, R, C>;

    fn new_service(&self, cfg: &C) -> Self::Future {
        match self {
            Either::A(ref inner) => EitherNewService::A(inner.new_service(cfg)),
            Either::B(ref inner) => EitherNewService::B(inner.new_service(cfg)),
        }
    }
}

impl<A: Clone, B: Clone> Clone for Either<A, B> {
    fn clone(&self) -> Self {
        match self {
            Either::A(srv) => Either::A(srv.clone()),
            Either::B(srv) => Either::B(srv.clone()),
        }
    }
}

#[doc(hidden)]
pub enum EitherNewService<A: NewService<R, C>, B: NewService<R, C>, R, C> {
    A(<A::Future as IntoFuture>::Future),
    B(<B::Future as IntoFuture>::Future),
}

impl<A, B, R, C> Future for EitherNewService<A, B, R, C>
where
    A: NewService<R, C>,
    B: NewService<R, C, Response = A::Response, Error = A::Error, InitError = A::InitError>,
{
    type Item = EitherService<A::Service, B::Service>;
    type Error = A::InitError;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self {
            EitherNewService::A(ref mut fut) => {
                let service = try_ready!(fut.poll());
                Ok(Async::Ready(EitherService::A(service)))
            }
            EitherNewService::B(ref mut fut) => {
                let service = try_ready!(fut.poll());
                Ok(Async::Ready(EitherService::B(service)))
            }
        }
    }
}
