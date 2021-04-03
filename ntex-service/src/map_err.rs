use std::{future::Future, marker::PhantomData, pin::Pin, task::Context, task::Poll};

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
    #[inline]
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

    #[inline]
    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx).map_err(&self.f)
    }

    #[inline]
    fn poll_shutdown(&self, cx: &mut Context<'_>, is_error: bool) -> Poll<()> {
        self.service.poll_shutdown(cx, is_error)
    }

    #[inline]
    fn call(&self, req: A::Request) -> Self::Future {
        MapErrFuture::new(self.service.call(req), self.f.clone())
    }
}

pin_project_lite::pin_project! {
pub struct MapErrFuture<A, F, E>
where
    A: Service,
    F: Fn(A::Error) -> E,
{
    f: F,
    #[pin]
    fut: A::Future,
}
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

    #[inline]
    fn new_service(&self, cfg: A::Config) -> Self::Future {
        MapErrServiceFuture::new(self.a.new_service(cfg), self.f.clone())
    }
}

pin_project_lite::pin_project! {
pub struct MapErrServiceFuture<A, F, E>
where
    A: ServiceFactory,
    F: Fn(A::Error) -> E,
{
    #[pin]
    fut: A::Future,
    f: F,
}
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
    use super::*;
    use crate::{IntoServiceFactory, Service, ServiceFactory};
    use ntex_util::future::{lazy, Ready};

    #[derive(Clone)]
    struct Srv;

    impl Service for Srv {
        type Request = ();
        type Response = ();
        type Error = ();
        type Future = Ready<(), ()>;

        fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Err(()))
        }

        fn call(&self, _: ()) -> Self::Future {
            Ready::Err(())
        }
    }

    #[ntex::test]
    async fn test_poll_ready() {
        let srv = Srv.map_err(|_| "error");
        let res = lazy(|cx| srv.poll_ready(cx)).await;
        assert_eq!(res, Poll::Ready(Err("error")));

        let res = lazy(|cx| srv.poll_shutdown(cx, true)).await;
        assert_eq!(res, Poll::Ready(()));
    }

    #[ntex::test]
    async fn test_service() {
        let srv = Srv.map_err(|_| "error").clone();
        let res = srv.call(()).await;
        assert!(res.is_err());
        assert_eq!(res.err().unwrap(), "error");
    }

    #[ntex::test]
    async fn test_pipeline() {
        let srv = crate::pipeline(Srv).map_err(|_| "error").clone();
        let res = srv.call(()).await;
        assert!(res.is_err());
        assert_eq!(res.err().unwrap(), "error");
    }

    #[ntex::test]
    async fn test_factory() {
        let new_srv = (|| Ready::<_, ()>::Ok(Srv))
            .into_factory()
            .map_err(|_| "error")
            .clone();
        let srv = new_srv.new_service(&()).await.unwrap();
        let res = srv.call(()).await;
        assert!(res.is_err());
        assert_eq!(res.err().unwrap(), "error");
    }

    #[ntex::test]
    async fn test_pipeline_factory() {
        let new_srv =
            crate::pipeline_factory((|| async { Ok::<_, ()>(Srv) }).into_factory())
                .map_err(|_| "error")
                .clone();
        let srv = new_srv.new_service(&()).await.unwrap();
        let res = srv.call(()).await;
        assert!(res.is_err());
        assert_eq!(res.err().unwrap(), "error");
    }
}
