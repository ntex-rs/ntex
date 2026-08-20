use std::{fmt, marker::PhantomData};

use super::{Ctx, Service, ServiceFactory};

/// Service for the `map_err` combinator, changing the type of a service's
/// error.
///
/// This is created by the `ServiceExt::map_err` method.
pub struct MapErr<F, S, E> {
    f: F,
    svc: S,
    e: PhantomData<E>,
}

impl<F, S, E> MapErr<F, S, E> {
    /// Create new `MapErr` combinator
    pub(crate) fn new<St, Req>(f: F, svc: S) -> Self
    where
        S: Service<St, Req>,
        F: Fn(S::Error) -> E,
    {
        Self {
            f,
            svc,
            e: PhantomData,
        }
    }
}

impl<F, S, E> Clone for MapErr<F, S, E>
where
    F: Clone,
    S: Clone,
{
    #[inline]
    fn clone(&self) -> Self {
        MapErr {
            f: self.f.clone(),
            svc: self.svc.clone(),
            e: PhantomData,
        }
    }
}

impl<F, S, E> fmt::Debug for MapErr<F, S, E>
where
    S: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MapErr")
            .field("svc", &self.svc)
            .field("map", &std::any::type_name::<F>())
            .finish()
    }
}

impl<F, S, St, Req, E> Service<St, Req> for MapErr<F, S, E>
where
    S: Service<St, Req>,
    F: Fn(S::Error) -> E,
{
    type Res = S::Res;
    type Error = E;

    #[inline]
    async fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> Result<S::Res, E> {
        ctx.call(&self.svc, req).await.map_err(|e| (self.f)(e))
    }

    #[inline]
    async fn ready(&self, ctx: Ctx<'_, Self, St>) -> Result<(), E> {
        ctx.ready(&self.svc).await.map_err(&self.f)
    }

    crate::forward_shutdown!(St, svc);
}

/// Factory for the `map_err` combinator, changing the type of a new
/// service's error.
///
/// This is created by the `ServiceFactory::map_err` method.
pub struct MapErrFactory<F, Sf, E> {
    f: F,
    sf: Sf,
    e: PhantomData<fn(Sf) -> E>,
}

impl<F, Sf, E> MapErrFactory<F, Sf, E> {
    /// Create new `MapErr` new service instance
    pub(crate) fn new<St, Req, Cfg>(f: F, sf: Sf) -> Self
    where
        Sf: ServiceFactory<St, Req, Cfg>,
        F: Fn(Sf::Error) -> E + Clone,
    {
        Self {
            f,
            sf,
            e: PhantomData,
        }
    }
}

impl<F: Clone, Sf: Clone, E> Clone for MapErrFactory<F, Sf, E> {
    fn clone(&self) -> Self {
        Self {
            f: self.f.clone(),
            sf: self.sf.clone(),
            e: PhantomData,
        }
    }
}

impl<F, Sf, E> fmt::Debug for MapErrFactory<F, Sf, E>
where
    Sf: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MapErrFactory")
            .field("sf", &self.sf)
            .field("map", &std::any::type_name::<F>())
            .finish()
    }
}

impl<F, Sf, St, Req, Cfg, E> ServiceFactory<St, Req, Cfg> for MapErrFactory<F, Sf, E>
where
    Sf: ServiceFactory<St, Req, Cfg>,
    F: Fn(Sf::Error) -> E + Clone,
{
    type Res = Sf::Res;
    type Error = E;

    type Service = MapErr<F, Sf::Service, E>;
    type InitError = Sf::InitError;

    #[inline]
    async fn create(&self, cfg: &Cfg) -> Result<Self::Service, Self::InitError> {
        self.sf.create(cfg).await.map(|svc| MapErr {
            svc,
            f: self.f.clone(),
            e: PhantomData,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use super::*;
    use crate::{Pipeline, fn_factory};

    #[derive(Debug, Clone)]
    struct Srv(bool, Rc<Cell<usize>>);

    impl Service<(), ()> for Srv {
        type Res = ();
        type Error = ();

        async fn ready(&self, _: Ctx<'_, Self>) -> Result<(), Self::Error> {
            if self.0 { Err(()) } else { Ok(()) }
        }

        async fn call(&self, _m: (), _: Ctx<'_, Self>) -> Result<(), ()> {
            Err(())
        }

        async fn shutdown(&self, _: Ctx<'_, Self, ()>) {
            self.1.set(self.1.get() + 1);
        }
    }

    #[ntex::test]
    async fn test_ready() {
        let cnt_sht = Rc::new(Cell::new(0));
        let srv = Pipeline::new(Srv(true, cnt_sht.clone()).map_err(|()| "error"));
        let res = srv.ready().await;
        assert_eq!(res, Err("error"));

        srv.shutdown().await;
        assert_eq!(cnt_sht.get(), 1);
    }

    #[ntex::test]
    async fn test_service() {
        let srv = Pipeline::new(
            Srv(false, Rc::new(Cell::new(0)))
                .map_err(|()| "error")
                .clone(),
        );
        let res = srv.call(()).await;
        assert!(res.is_err());
        assert_eq!(res.err().unwrap(), "error");

        let _ = format!("{srv:?}");
    }

    #[ntex::test]
    async fn test_pipeline() {
        let srv = Pipeline::new(
            crate::svc(Srv(false, Rc::new(Cell::new(0))))
                .map_err(|()| "error")
                .clone(),
        );
        let res = srv.call(()).await;
        assert!(res.is_err());
        assert_eq!(res.err().unwrap(), "error");

        let _ = format!("{srv:?}");
    }

    #[ntex::test]
    async fn test_factory() {
        let new_srv = fn_factory(|| async { Ok::<_, ()>(Srv(false, Rc::new(Cell::new(0)))) })
            .map_err(|()| "error")
            .clone();
        let srv = Pipeline::new(new_srv.create(&()).await.unwrap());
        let res = srv.call(()).await;
        assert!(res.is_err());
        assert_eq!(res.err().unwrap(), "error");
        let _ = format!("{new_srv:?}");
    }

    #[ntex::test]
    async fn test_pipeline_factory() {
        let new_srv = crate::factory(fn_factory(|| async {
            Ok::<Srv, ()>(Srv(false, Rc::new(Cell::new(0))))
        }))
        .map_err(|()| "error")
        .clone();
        let srv = Pipeline::new(new_srv.create(&()).await.unwrap());
        let res = srv.call(()).await;
        assert!(res.is_err());
        assert_eq!(res.err().unwrap(), "error");
        let _ = format!("{new_srv:?}");
    }
}
