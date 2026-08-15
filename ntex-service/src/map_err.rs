use std::{fmt, marker::PhantomData};

use super::{Ctx, ReadyCtx, Service, ServiceFactory};

/// Service for the `map_err` combinator, changing the type of a service's
/// error.
///
/// This is created by the `ServiceExt::map_err` method.
pub struct MapErr<S, F, E> {
    svc: S,
    f: F,
    _t: PhantomData<E>,
}

impl<S, F, E> MapErr<S, F, E> {
    /// Create new `MapErr` combinator
    pub(crate) fn new(svc: S, f: F) -> Self
    where
        S: Service,
        F: Fn(S::Error) -> E,
    {
        Self {
            svc,
            f,
            _t: PhantomData,
        }
    }
}

impl<S, F, E> Clone for MapErr<S, F, E>
where
    S: Clone,
    F: Clone,
{
    #[inline]
    fn clone(&self) -> Self {
        MapErr {
            svc: self.svc.clone(),
            f: self.f.clone(),
            _t: PhantomData,
        }
    }
}

impl<S, F, E> fmt::Debug for MapErr<S, F, E>
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

impl<S, F, E> Service for MapErr<S, F, E>
where
    S: Service,
    F: Fn(S::Error) -> E,
{
    type St = S::St;
    type Req = S::Req;
    type Res = S::Res;
    type Error = E;

    #[inline]
    async fn call(&self, req: S::Req, ctx: Ctx<'_, Self>) -> Result<S::Res, E> {
        ctx.call(&self.svc, req).await.map_err(|e| (self.f)(e))
    }

    #[inline]
    async fn ready(&self, ctx: ReadyCtx<'_, Self>) -> Result<(), E> {
        ctx.ready(&self.svc).await.map_err(&self.f)
    }

    crate::forward_shutdown!(svc);
}

/// Factory for the `map_err` combinator, changing the type of a new
/// service's error.
///
/// This is created by the `ServiceFactory::map_err` method.
pub struct MapErrFactory<Sf, F, E> {
    sf: Sf,
    f: F,
    e: PhantomData<fn(Sf) -> E>,
}

impl<Sf, F, E> MapErrFactory<Sf, F, E> {
    /// Create new `MapErr` new service instance
    pub(crate) fn new<Req>(sf: Sf, f: F) -> Self
    where
        Sf: ServiceFactory<Req>,
        F: Fn(Sf::Error) -> E + Clone,
    {
        Self {
            sf,
            f,
            e: PhantomData,
        }
    }
}

impl<Sf: Clone, F: Clone, E> Clone for MapErrFactory<Sf, F, E> {
    fn clone(&self) -> Self {
        Self {
            sf: self.sf.clone(),
            f: self.f.clone(),
            e: PhantomData,
        }
    }
}

impl<Sf, F, E> fmt::Debug for MapErrFactory<Sf, F, E>
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

impl<Sf, Req, F, E> ServiceFactory<Req> for MapErrFactory<Sf, F, E>
where
    Sf: ServiceFactory<Req>,
    F: Fn(Sf::Error) -> E + Clone,
{
    type St = Sf::St;
    type Res = Sf::Res;
    type Error = E;

    type Service = MapErr<Sf::Service, F, E>;
    type InitCfg = Sf::InitCfg;
    type InitError = Sf::InitError;

    #[inline]
    async fn create(&self, cfg: &Sf::InitCfg) -> Result<Self::Service, Self::InitError> {
        self.sf.create(cfg).await.map(|svc| MapErr {
            svc,
            f: self.f.clone(),
            _t: PhantomData,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unused_async_trait_impl)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use super::*;
    use crate::{Pipeline, fn_factory};

    #[derive(Debug, Clone)]
    struct Srv(bool, Rc<Cell<usize>>);

    impl Service for Srv {
        type St = ();
        type Req = ();
        type Res = ();
        type Error = ();

        async fn ready(&self, _: ReadyCtx<'_, Self>) -> Result<(), Self::Error> {
            if self.0 { Err(()) } else { Ok(()) }
        }

        async fn call(&self, _m: (), _: Ctx<'_, Self>) -> Result<(), ()> {
            Err(())
        }

        async fn shutdown(&self) {
            self.1.set(self.1.get() + 1);
        }
    }

    #[ntex::test]
    async fn test_ready() {
        let cnt_sht = Rc::new(Cell::new(0));
        let srv = Pipeline::new(Srv(true, cnt_sht.clone()).map_err(|()| "error"));
        let res = srv.ready(&()).await;
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
        let res = srv.call((), &()).await;
        assert!(res.is_err());
        assert_eq!(res.err().unwrap(), "error");

        let _ = format!("{srv:?}");
    }

    #[ntex::test]
    async fn test_pipeline() {
        let srv = Pipeline::new(
            crate::chain(Srv(false, Rc::new(Cell::new(0))))
                .map_err(|()| "error")
                .clone(),
        );
        let res = srv.call((), &()).await;
        assert!(res.is_err());
        assert_eq!(res.err().unwrap(), "error");

        let _ = format!("{srv:?}");
    }

    #[ntex::test]
    async fn test_factory() {
        let new_srv =
            fn_factory(|| async { Ok::<_, ()>(Srv(false, Rc::new(Cell::new(0)))) })
                .map_err(|()| "error")
                .clone();
        let srv = Pipeline::new(new_srv.create(&()).await.unwrap());
        let res = srv.call((), &()).await;
        assert!(res.is_err());
        assert_eq!(res.err().unwrap(), "error");
        let _ = format!("{new_srv:?}");
    }

    #[ntex::test]
    async fn test_pipeline_factory() {
        let new_srv = crate::chain_factory(fn_factory(|| async {
            Ok::<Srv, ()>(Srv(false, Rc::new(Cell::new(0))))
        }))
        .map_err(|()| "error")
        .clone();
        let srv = Pipeline::new(new_srv.create(&()).await.unwrap());
        let res = srv.call((), &()).await;
        assert!(res.is_err());
        assert_eq!(res.err().unwrap(), "error");
        let _ = format!("{new_srv:?}");
    }
}
