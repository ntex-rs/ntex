use std::{future::Future, marker::PhantomData, pin::Pin, task::Context, task::Poll};

use super::{Ctx, Service, ServiceCall, ServiceFactory};

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
    pub(crate) fn new<R>(service: A, f: F) -> Self
    where
        A: Service<R>,
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

impl<A, R, F, E> Service<R> for MapErr<A, F, E>
where
    A: Service<R>,
    F: Fn(A::Error) -> E,
{
    type Response = A::Response;
    type Error = E;
    type Future<'f> = MapErrFuture<'f, A, R, F, E> where A: 'f, R: 'f, F: 'f, E: 'f;

    #[inline]
    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx).map_err(&self.f)
    }

    #[inline]
    fn call<'a>(&'a self, req: R, ctx: Ctx<'a, Self>) -> Self::Future<'a> {
        MapErrFuture {
            slf: self,
            fut: ctx.call(&self.service, req),
        }
    }

    crate::forward_poll_shutdown!(service);
}

pin_project_lite::pin_project! {
    #[must_use = "futures do nothing unless polled"]
    pub struct MapErrFuture<'f, A, R, F, E>
    where
        A: Service<R>,
        A: 'f,
        R: 'f,
        F: Fn(A::Error) -> E,
    {
        slf: &'f MapErr<A, F, E>,
        #[pin]
        fut: ServiceCall<'f, A, R>,
    }
}

impl<'f, A, R, F, E> Future for MapErrFuture<'f, A, R, F, E>
where
    A: Service<R> + 'f,
    F: Fn(A::Error) -> E,
{
    type Output = Result<A::Response, E>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.as_mut().project();
        this.fut.poll(cx).map_err(|e| (self.project().slf.f)(e))
    }
}

/// Factory for the `map_err` combinator, changing the type of a new
/// service's error.
///
/// This is created by the `NewServiceExt::map_err` method.
pub struct MapErrFactory<A, R, C, F, E>
where
    A: ServiceFactory<R, C>,
    F: Fn(A::Error) -> E + Clone,
{
    a: A,
    f: F,
    e: PhantomData<fn(R, C) -> E>,
}

impl<A, R, C, F, E> MapErrFactory<A, R, C, F, E>
where
    A: ServiceFactory<R, C>,
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

impl<A, R, C, F, E> Clone for MapErrFactory<A, R, C, F, E>
where
    A: ServiceFactory<R, C> + Clone,
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

impl<A, R, C, F, E> ServiceFactory<R, C> for MapErrFactory<A, R, C, F, E>
where
    A: ServiceFactory<R, C>,
    F: Fn(A::Error) -> E + Clone,
{
    type Response = A::Response;
    type Error = E;

    type Service = MapErr<A::Service, F, E>;
    type InitError = A::InitError;
    type Future<'f> = MapErrFactoryFuture<'f, A, R, C, F, E> where Self: 'f, C: 'f;

    #[inline]
    fn create(&self, cfg: C) -> Self::Future<'_> {
        MapErrFactoryFuture {
            f: self.f.clone(),
            fut: self.a.create(cfg),
        }
    }
}

pin_project_lite::pin_project! {
    #[must_use = "futures do nothing unless polled"]
    pub struct MapErrFactoryFuture<'f, A, R, C, F, E>
    where
        A: ServiceFactory<R, C>,
        A: 'f,
        F: Fn(A::Error) -> E,
        C: 'f,
    {
        f: F,
        #[pin]
        fut: A::Future<'f>,
    }
}

impl<'f, A, R, C, F, E> Future for MapErrFactoryFuture<'f, A, R, C, F, E>
where
    A: ServiceFactory<R, C>,
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
    use ntex_util::future::{lazy, Ready};

    use super::*;
    use crate::{fn_factory, Container, Ctx, Service, ServiceFactory};

    #[derive(Clone)]
    struct Srv(bool);

    impl Service<()> for Srv {
        type Response = ();
        type Error = ();
        type Future<'f> = Ready<(), ()>;

        fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            if self.0 {
                Poll::Ready(Err(()))
            } else {
                Poll::Ready(Ok(()))
            }
        }

        fn call<'a>(&'a self, _: (), _: Ctx<'a, Self>) -> Self::Future<'a> {
            Ready::Err(())
        }
    }

    #[ntex::test]
    async fn test_poll_ready() {
        let srv = Srv(true).map_err(|_| "error");
        let res = lazy(|cx| srv.poll_ready(cx)).await;
        assert_eq!(res, Poll::Ready(Err("error")));

        let res = lazy(|cx| srv.poll_shutdown(cx)).await;
        assert_eq!(res, Poll::Ready(()));
    }

    #[ntex::test]
    async fn test_service() {
        let srv = Container::new(Srv(false).map_err(|_| "error").clone());
        let res = srv.call(()).await;
        assert!(res.is_err());
        assert_eq!(res.err().unwrap(), "error");
    }

    #[ntex::test]
    async fn test_pipeline() {
        let srv = Container::new(crate::pipeline(Srv(false)).map_err(|_| "error").clone());
        let res = srv.call(()).await;
        assert!(res.is_err());
        assert_eq!(res.err().unwrap(), "error");
    }

    #[ntex::test]
    async fn test_factory() {
        let new_srv = fn_factory(|| Ready::<_, ()>::Ok(Srv(false)))
            .map_err(|_| "error")
            .clone();
        let srv = Container::new(new_srv.create(&()).await.unwrap());
        let res = srv.call(()).await;
        assert!(res.is_err());
        assert_eq!(res.err().unwrap(), "error");
    }

    #[ntex::test]
    async fn test_pipeline_factory() {
        let new_srv =
            crate::pipeline_factory(fn_factory(|| async { Ok::<Srv, ()>(Srv(false)) }))
                .map_err(|_| "error")
                .clone();
        let srv = Container::new(new_srv.create(&()).await.unwrap());
        let res = srv.call(()).await;
        assert!(res.is_err());
        assert_eq!(res.err().unwrap(), "error");
    }
}
