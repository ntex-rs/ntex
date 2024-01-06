use std::{fmt, marker::PhantomData, task::Context, task::Poll};

use super::{Service, ServiceCtx, ServiceFactory};

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

impl<A, F, E> fmt::Debug for MapErr<A, F, E>
where
    A: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MapErr")
            .field("svc", &self.service)
            .field("map", &std::any::type_name::<F>())
            .finish()
    }
}

impl<A, R, F, E> Service<R> for MapErr<A, F, E>
where
    A: Service<R>,
    F: Fn(A::Error) -> E,
{
    type Response = A::Response;
    type Error = E;

    #[inline]
    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx).map_err(&self.f)
    }

    #[inline]
    async fn call(
        &self,
        req: R,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        ctx.call(&self.service, req).await.map_err(|e| (self.f)(e))
    }

    crate::forward_poll_shutdown!(service);
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

impl<A, R, C, F, E> fmt::Debug for MapErrFactory<A, R, C, F, E>
where
    A: ServiceFactory<R, C> + fmt::Debug,
    F: Fn(A::Error) -> E + Clone,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MapErrFactory")
            .field("factory", &self.a)
            .field("map", &std::any::type_name::<F>())
            .finish()
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

    #[inline]
    async fn create(&self, cfg: C) -> Result<Self::Service, Self::InitError> {
        self.a.create(cfg).await.map(|service| MapErr {
            service,
            f: self.f.clone(),
            _t: PhantomData,
        })
    }
}

#[cfg(test)]
mod tests {
    use ntex_util::future::{lazy, Ready};

    use super::*;
    use crate::{fn_factory, Pipeline, Service, ServiceCtx, ServiceFactory};

    #[derive(Debug, Clone)]
    struct Srv(bool);

    impl Service<()> for Srv {
        type Response = ();
        type Error = ();

        fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            if self.0 {
                Poll::Ready(Err(()))
            } else {
                Poll::Ready(Ok(()))
            }
        }

        async fn call<'a>(&'a self, _: (), _: ServiceCtx<'a, Self>) -> Result<(), ()> {
            Err(())
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
        let srv = Pipeline::new(Srv(false).map_err(|_| "error").clone());
        let res = srv.call(()).await;
        assert!(res.is_err());
        assert_eq!(res.err().unwrap(), "error");

        format!("{:?}", srv);
    }

    #[ntex::test]
    async fn test_pipeline() {
        let srv = Pipeline::new(crate::chain(Srv(false)).map_err(|_| "error").clone());
        let res = srv.call(()).await;
        assert!(res.is_err());
        assert_eq!(res.err().unwrap(), "error");

        format!("{:?}", srv);
    }

    #[ntex::test]
    async fn test_factory() {
        let new_srv = fn_factory(|| Ready::<_, ()>::Ok(Srv(false)))
            .map_err(|_| "error")
            .clone();
        let srv = Pipeline::new(new_srv.create(&()).await.unwrap());
        let res = srv.call(()).await;
        assert!(res.is_err());
        assert_eq!(res.err().unwrap(), "error");
        format!("{:?}", new_srv);
    }

    #[ntex::test]
    async fn test_pipeline_factory() {
        let new_srv =
            crate::chain_factory(fn_factory(|| async { Ok::<Srv, ()>(Srv(false)) }))
                .map_err(|_| "error")
                .clone();
        let srv = Pipeline::new(new_srv.create(&()).await.unwrap());
        let res = srv.call(()).await;
        assert!(res.is_err());
        assert_eq!(res.err().unwrap(), "error");
        format!("{:?}", new_srv);
    }
}
