//! Contains `Either` service and related types and functions.
use std::pin::Pin;
use std::task::{Context, Poll};

use actix_service::{Service, ServiceFactory};
use futures::{future, ready, Future};
use pin_project::pin_project;

/// Combine two different service types into a single type.
///
/// Both services must be of the same request, response, and error types.
/// `EitherService` is useful for handling conditional branching in service
/// middleware to different inner service types.
pub struct EitherService<A, B> {
    left: A,
    right: B,
}

impl<A: Clone, B: Clone> Clone for EitherService<A, B> {
    fn clone(&self) -> Self {
        EitherService {
            left: self.left.clone(),
            right: self.right.clone(),
        }
    }
}

impl<A, B> Service for EitherService<A, B>
where
    A: Service,
    B: Service<Response = A::Response, Error = A::Error>,
{
    type Request = either::Either<A::Request, B::Request>;
    type Response = A::Response;
    type Error = A::Error;
    type Future = future::Either<A::Future, B::Future>;

    fn poll_ready(&mut self, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        let left = self.left.poll_ready(cx)?;
        let right = self.right.poll_ready(cx)?;

        if left.is_ready() && right.is_ready() {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }

    fn call(&mut self, req: either::Either<A::Request, B::Request>) -> Self::Future {
        match req {
            either::Either::Left(req) => future::Either::Left(self.left.call(req)),
            either::Either::Right(req) => future::Either::Right(self.right.call(req)),
        }
    }
}

/// Combine two different new service types into a single service.
pub struct Either<A, B> {
    left: A,
    right: B,
}

impl<A, B> Either<A, B> {
    pub fn new(left: A, right: B) -> Either<A, B>
    where
        A: ServiceFactory,
        B: ServiceFactory<
            Config = A::Config,
            Response = A::Response,
            Error = A::Error,
            InitError = A::InitError,
        >,
    {
        Either { left, right }
    }
}

impl<A, B> ServiceFactory for Either<A, B>
where
    A: ServiceFactory,
    B: ServiceFactory<
        Config = A::Config,
        Response = A::Response,
        Error = A::Error,
        InitError = A::InitError,
    >,
{
    type Request = either::Either<A::Request, B::Request>;
    type Response = A::Response;
    type Error = A::Error;
    type InitError = A::InitError;
    type Config = A::Config;
    type Service = EitherService<A::Service, B::Service>;
    type Future = EitherNewService<A, B>;

    fn new_service(&self, cfg: &A::Config) -> Self::Future {
        EitherNewService {
            left: None,
            right: None,
            left_fut: self.left.new_service(cfg),
            right_fut: self.right.new_service(cfg),
        }
    }
}

impl<A: Clone, B: Clone> Clone for Either<A, B> {
    fn clone(&self) -> Self {
        Self {
            left: self.left.clone(),
            right: self.right.clone(),
        }
    }
}

#[pin_project]
#[doc(hidden)]
pub struct EitherNewService<A: ServiceFactory, B: ServiceFactory> {
    left: Option<A::Service>,
    right: Option<B::Service>,
    #[pin]
    left_fut: A::Future,
    #[pin]
    right_fut: B::Future,
}

impl<A, B> Future for EitherNewService<A, B>
where
    A: ServiceFactory,
    B: ServiceFactory<Response = A::Response, Error = A::Error, InitError = A::InitError>,
{
    type Output = Result<EitherService<A::Service, B::Service>, A::InitError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let this = self.project();

        if this.left.is_none() {
            *this.left = Some(ready!(this.left_fut.poll(cx))?);
        }
        if this.right.is_none() {
            *this.right = Some(ready!(this.right_fut.poll(cx))?);
        }

        if this.left.is_some() && this.right.is_some() {
            Poll::Ready(Ok(EitherService {
                left: this.left.take().unwrap(),
                right: this.right.take().unwrap(),
            }))
        } else {
            Poll::Pending
        }
    }
}
