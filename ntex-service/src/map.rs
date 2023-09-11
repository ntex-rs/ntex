use std::{fmt, future::Future, marker::PhantomData, pin::Pin, task::Context, task::Poll};

use super::{Service, ServiceCall, ServiceCtx, ServiceFactory};

/// Service for the `map` combinator, changing the type of a service's response.
///
/// This is created by the `ServiceExt::map` method.
pub struct Map<A, F, Req, Res> {
    service: A,
    f: F,
    _t: PhantomData<fn(Req) -> Res>,
}

impl<A, F, Req, Res> Map<A, F, Req, Res> {
    /// Create new `Map` combinator
    pub(crate) fn new(service: A, f: F) -> Self
    where
        A: Service<Req>,
        F: Fn(A::Response) -> Res,
    {
        Self {
            service,
            f,
            _t: PhantomData,
        }
    }
}

impl<A, F, Req, Res> Clone for Map<A, F, Req, Res>
where
    A: Clone,
    F: Clone,
{
    #[inline]
    fn clone(&self) -> Self {
        Map {
            service: self.service.clone(),
            f: self.f.clone(),
            _t: PhantomData,
        }
    }
}

impl<A, F, Req, Res> fmt::Debug for Map<A, F, Req, Res>
where
    A: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Map")
            .field("service", &self.service)
            .field("map", &std::any::type_name::<F>())
            .finish()
    }
}

impl<A, F, Req, Res> Service<Req> for Map<A, F, Req, Res>
where
    A: Service<Req>,
    F: Fn(A::Response) -> Res,
{
    type Response = Res;
    type Error = A::Error;
    type Future<'f> = MapFuture<'f, A, F, Req, Res> where Self: 'f, Req: 'f;

    crate::forward_poll_ready!(service);
    crate::forward_poll_shutdown!(service);

    #[inline]
    fn call<'a>(&'a self, req: Req, ctx: ServiceCtx<'a, Self>) -> Self::Future<'a> {
        MapFuture {
            fut: ctx.call(&self.service, req),
            slf: self,
        }
    }
}

pin_project_lite::pin_project! {
    #[must_use = "futures do nothing unless polled"]
    pub struct MapFuture<'f, A, F, Req, Res>
    where
        A: Service<Req>,
        A: 'f,
        Req: 'f,
        F: Fn(A::Response) -> Res,
    {
        slf: &'f Map<A, F, Req, Res>,
        #[pin]
        fut: ServiceCall<'f, A, Req>,
    }
}

impl<'f, A, F, Req, Res> Future for MapFuture<'f, A, F, Req, Res>
where
    A: Service<Req> + 'f,
    Req: 'f,
    F: Fn(A::Response) -> Res,
{
    type Output = Result<Res, A::Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.as_mut().project();

        match this.fut.poll(cx) {
            Poll::Ready(Ok(resp)) => Poll::Ready(Ok((self.project().slf.f)(resp))),
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// `MapNewService` new service combinator
pub struct MapFactory<A, F, Req, Res, Cfg> {
    a: A,
    f: F,
    r: PhantomData<fn(Req, Cfg) -> Res>,
}

impl<A, F, Req, Res, Cfg> MapFactory<A, F, Req, Res, Cfg>
where
    A: ServiceFactory<Req, Cfg>,
    F: Fn(A::Response) -> Res,
{
    /// Create new `Map` new service instance
    pub(crate) fn new(a: A, f: F) -> Self {
        Self {
            a,
            f,
            r: PhantomData,
        }
    }
}

impl<A, F, Req, Res, Cfg> Clone for MapFactory<A, F, Req, Res, Cfg>
where
    A: Clone,
    F: Clone,
{
    #[inline]
    fn clone(&self) -> Self {
        Self {
            a: self.a.clone(),
            f: self.f.clone(),
            r: PhantomData,
        }
    }
}

impl<A, F, Req, Res, Cfg> fmt::Debug for MapFactory<A, F, Req, Res, Cfg>
where
    A: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MapFactory")
            .field("factory", &self.a)
            .field("map", &std::any::type_name::<F>())
            .finish()
    }
}

impl<A, F, Req, Res, Cfg> ServiceFactory<Req, Cfg> for MapFactory<A, F, Req, Res, Cfg>
where
    A: ServiceFactory<Req, Cfg>,
    F: Fn(A::Response) -> Res + Clone,
{
    type Response = Res;
    type Error = A::Error;

    type Service = Map<A::Service, F, Req, Res>;
    type InitError = A::InitError;
    type Future<'f> = MapFactoryFuture<'f, A, F, Req, Res, Cfg> where Self: 'f, Cfg: 'f;

    #[inline]
    fn create(&self, cfg: Cfg) -> Self::Future<'_> {
        MapFactoryFuture {
            fut: self.a.create(cfg),
            f: Some(self.f.clone()),
        }
    }
}

pin_project_lite::pin_project! {
    #[must_use = "futures do nothing unless polled"]
    pub struct MapFactoryFuture<'f, A, F, Req, Res, Cfg>
    where
        A: ServiceFactory<Req, Cfg>,
        A: 'f,
        F: Fn(A::Response) -> Res,
        Cfg: 'f,
    {
        #[pin]
        fut: A::Future<'f>,
        f: Option<F>,
    }
}

impl<'f, A, F, Req, Res, Cfg> Future for MapFactoryFuture<'f, A, F, Req, Res, Cfg>
where
    A: ServiceFactory<Req, Cfg>,
    F: Fn(A::Response) -> Res,
{
    type Output = Result<Map<A::Service, F, Req, Res>, A::InitError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        if let Poll::Ready(svc) = this.fut.poll(cx)? {
            Poll::Ready(Ok(Map::new(svc, this.f.take().unwrap())))
        } else {
            Poll::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use ntex_util::future::{lazy, Ready};

    use super::*;
    use crate::{fn_factory, Pipeline, Service, ServiceCtx, ServiceFactory};

    #[derive(Debug, Clone)]
    struct Srv;

    impl Service<()> for Srv {
        type Response = ();
        type Error = ();
        type Future<'f> = Ready<(), ()>;

        fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call<'a>(&'a self, _: (), _: ServiceCtx<'a, Self>) -> Self::Future<'a> {
            Ready::Ok(())
        }
    }

    #[ntex::test]
    async fn test_service() {
        let srv = Pipeline::new(Srv.map(|_| "ok").clone());
        let res = srv.call(()).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "ok");

        let res = lazy(|cx| srv.poll_ready(cx)).await;
        assert_eq!(res, Poll::Ready(Ok(())));

        let res = lazy(|cx| srv.poll_shutdown(cx)).await;
        assert_eq!(res, Poll::Ready(()));

        format!("{:?}", srv);
    }

    #[ntex::test]
    async fn test_pipeline() {
        let srv = Pipeline::new(crate::chain(Srv).map(|_| "ok").clone());
        let res = srv.call(()).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "ok");

        let res = lazy(|cx| srv.poll_ready(cx)).await;
        assert_eq!(res, Poll::Ready(Ok(())));

        let res = lazy(|cx| srv.poll_shutdown(cx)).await;
        assert_eq!(res, Poll::Ready(()));
    }

    #[ntex::test]
    async fn test_factory() {
        let new_srv = fn_factory(|| async { Ok::<_, ()>(Srv) })
            .map(|_| "ok")
            .clone();
        let srv = Pipeline::new(new_srv.create(&()).await.unwrap());
        let res = srv.call(()).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("ok"));

        format!("{:?}", new_srv);
    }

    #[ntex::test]
    async fn test_pipeline_factory() {
        let new_srv = crate::chain_factory(fn_factory(|| async { Ok::<_, ()>(Srv) }))
            .map(|_| "ok")
            .clone();
        let srv = Pipeline::new(new_srv.create(&()).await.unwrap());
        let res = srv.call(()).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("ok"));

        format!("{:?}", new_srv);
    }
}
