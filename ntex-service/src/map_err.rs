use std::{fmt, marker::PhantomData};

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
    async fn ready(&self, ctx: ServiceCtx<'_, Self>) -> Result<(), Self::Error> {
        ctx.ready(&self.service).await.map_err(&self.f)
    }

    #[inline]
    async fn call(
        &self,
        req: R,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        ctx.call(&self.service, req).await.map_err(|e| (self.f)(e))
    }

    crate::forward_shutdown!(service);
    crate::forward_notready!(service);
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
    use std::{cell::Cell, rc::Rc};

    use super::*;
    use crate::{fn_factory, Pipeline};

    #[derive(Debug, Clone)]
    struct Srv(bool, Rc<Cell<usize>>);

    impl Service<()> for Srv {
        type Response = ();
        type Error = ();

        async fn ready(&self, _: ServiceCtx<'_, Self>) -> Result<(), Self::Error> {
            if self.0 {
                Err(())
            } else {
                Ok(())
            }
        }

        async fn call(&self, _: (), _: ServiceCtx<'_, Self>) -> Result<(), ()> {
            Err(())
        }

        async fn shutdown(&self) {
            self.1.set(self.1.get() + 1);
        }
    }

    #[ntex::test]
    async fn test_ready() {
        let cnt_sht = Rc::new(Cell::new(0));
        let srv = Pipeline::new(Srv(true, cnt_sht.clone()).map_err(|_| "error"));
        let res = srv.ready().await;
        assert_eq!(res, Err("error"));

        srv.shutdown().await;
        assert_eq!(cnt_sht.get(), 1);
    }

    #[ntex::test]
    async fn test_service() {
        let srv = Pipeline::new(
            Srv(false, Rc::new(Cell::new(0)))
                .map_err(|_| "error")
                .clone(),
        );
        let res = srv.call(()).await;
        assert!(res.is_err());
        assert_eq!(res.err().unwrap(), "error");

        let _ = format!("{:?}", srv);
    }

    #[ntex::test]
    async fn test_pipeline() {
        let srv = Pipeline::new(
            crate::chain(Srv(false, Rc::new(Cell::new(0))))
                .map_err(|_| "error")
                .clone(),
        );
        let res = srv.call(()).await;
        assert!(res.is_err());
        assert_eq!(res.err().unwrap(), "error");

        let _ = format!("{:?}", srv);
    }

    #[ntex::test]
    async fn test_factory() {
        let new_srv =
            fn_factory(|| async { Ok::<_, ()>(Srv(false, Rc::new(Cell::new(0)))) })
                .map_err(|_| "error")
                .clone();
        let srv = Pipeline::new(new_srv.create(&()).await.unwrap());
        let res = srv.call(()).await;
        assert!(res.is_err());
        assert_eq!(res.err().unwrap(), "error");
        let _ = format!("{:?}", new_srv);
    }

    #[ntex::test]
    async fn test_pipeline_factory() {
        let new_srv = crate::chain_factory(fn_factory(|| async {
            Ok::<Srv, ()>(Srv(false, Rc::new(Cell::new(0))))
        }))
        .map_err(|_| "error")
        .clone();
        let srv = Pipeline::new(new_srv.create(&()).await.unwrap());
        let res = srv.call(()).await;
        assert!(res.is_err());
        assert_eq!(res.err().unwrap(), "error");
        let _ = format!("{:?}", new_srv);
    }
}
