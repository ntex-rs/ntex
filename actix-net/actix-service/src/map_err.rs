use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};

use super::{Service, ServiceFactory};

/// Service for the `map_err` combinator, changing the type of a service's
/// error.
///
/// This is created by the `ServiceExt::map_err` method.
pub struct MapErr<A, F, E> {
    service: A,
    f: F,
    _t: PhantomData<E>,
}

impl<A, F, E> MapErr<A, F, E> {
    /// Create new `MapErr` combinator
    pub(crate) fn new(service: A, f: F) -> Self
    where
        A: Service,
        F: Fn(A::Error) -> E,
    {
        Self {
            service,
            f,
            _t: PhantomData,
        }
    }
}

impl<A, F, E> Clone for MapErr<A, F, E>
where
    A: Clone,
    F: Clone,
{
    fn clone(&self) -> Self {
        MapErr {
            service: self.service.clone(),
            f: self.f.clone(),
            _t: PhantomData,
        }
    }
}

impl<A, F, E> Service for MapErr<A, F, E>
where
    A: Service,
    F: Fn(A::Error) -> E + Clone,
{
    type Request = A::Request;
    type Response = A::Response;
    type Error = E;
    type Future = MapErrFuture<A, F, E>;

    fn poll_ready(&mut self, ctx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(ctx).map_err(&self.f)
    }

    fn call(&mut self, req: A::Request) -> Self::Future {
        MapErrFuture::new(self.service.call(req), self.f.clone())
    }
}

#[pin_project::pin_project]
pub struct MapErrFuture<A, F, E>
where
    A: Service,
    F: Fn(A::Error) -> E,
{
    f: F,
    #[pin]
    fut: A::Future,
}

impl<A, F, E> MapErrFuture<A, F, E>
where
    A: Service,
    F: Fn(A::Error) -> E,
{
    fn new(fut: A::Future, f: F) -> Self {
        MapErrFuture { f, fut }
    }
}

impl<A, F, E> Future for MapErrFuture<A, F, E>
where
    A: Service,
    F: Fn(A::Error) -> E,
{
    type Output = Result<A::Response, E>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        this.fut.poll(cx).map_err(this.f)
    }
}

/// Factory for the `map_err` combinator, changing the type of a new
/// service's error.
///
/// This is created by the `NewServiceExt::map_err` method.
pub struct MapErrServiceFactory<A, F, E>
where
    A: ServiceFactory,
    F: Fn(A::Error) -> E + Clone,
{
    a: A,
    f: F,
    e: PhantomData<E>,
}

impl<A, F, E> MapErrServiceFactory<A, F, E>
where
    A: ServiceFactory,
    F: Fn(A::Error) -> E + Clone,
{
    /// Create new `MapErr` new service instance
    pub(crate) fn new(a: A, f: F) -> Self {
        Self {
            a,
            f,
            e: PhantomData,
        }
    }
}

impl<A, F, E> Clone for MapErrServiceFactory<A, F, E>
where
    A: ServiceFactory + Clone,
    F: Fn(A::Error) -> E + Clone,
{
    fn clone(&self) -> Self {
        Self {
            a: self.a.clone(),
            f: self.f.clone(),
            e: PhantomData,
        }
    }
}

impl<A, F, E> ServiceFactory for MapErrServiceFactory<A, F, E>
where
    A: ServiceFactory,
    F: Fn(A::Error) -> E + Clone,
{
    type Request = A::Request;
    type Response = A::Response;
    type Error = E;

    type Config = A::Config;
    type Service = MapErr<A::Service, F, E>;
    type InitError = A::InitError;
    type Future = MapErrServiceFuture<A, F, E>;

    fn new_service(&self, cfg: A::Config) -> Self::Future {
        MapErrServiceFuture::new(self.a.new_service(cfg), self.f.clone())
    }
}

#[pin_project::pin_project]
pub struct MapErrServiceFuture<A, F, E>
where
    A: ServiceFactory,
    F: Fn(A::Error) -> E,
{
    #[pin]
    fut: A::Future,
    f: F,
}

impl<A, F, E> MapErrServiceFuture<A, F, E>
where
    A: ServiceFactory,
    F: Fn(A::Error) -> E,
{
    fn new(fut: A::Future, f: F) -> Self {
        MapErrServiceFuture { f, fut }
    }
}

impl<A, F, E> Future for MapErrServiceFuture<A, F, E>
where
    A: ServiceFactory,
    F: Fn(A::Error) -> E + Clone,
{
    type Output = Result<MapErr<A::Service, F, E>, A::InitError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        if let Poll::Ready(svc) = this.fut.poll(cx)? {
            Poll::Ready(Ok(MapErr::new(svc, this.f.clone())))
        } else {
            Poll::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use futures_util::future::{err, lazy, ok, Ready};

    use super::*;
    use crate::{IntoServiceFactory, Service, ServiceFactory};

    struct Srv;

    impl Service for Srv {
        type Request = ();
        type Response = ();
        type Error = ();
        type Future = Ready<Result<(), ()>>;

        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Err(()))
        }

        fn call(&mut self, _: ()) -> Self::Future {
            err(())
        }
    }

    #[actix_rt::test]
    async fn test_poll_ready() {
        let mut srv = Srv.map_err(|_| "error");
        let res = lazy(|cx| srv.poll_ready(cx)).await;
        assert_eq!(res, Poll::Ready(Err("error")));
    }

    #[actix_rt::test]
    async fn test_call() {
        let mut srv = Srv.map_err(|_| "error");
        let res = srv.call(()).await;
        assert!(res.is_err());
        assert_eq!(res.err().unwrap(), "error");
    }

    #[actix_rt::test]
    async fn test_new_service() {
        let new_srv = (|| ok::<_, ()>(Srv)).into_factory().map_err(|_| "error");
        let mut srv = new_srv.new_service(&()).await.unwrap();
        let res = srv.call(()).await;
        assert!(res.is_err());
        assert_eq!(res.err().unwrap(), "error");
    }
}
