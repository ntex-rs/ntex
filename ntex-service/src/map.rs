use std::{fmt, marker::PhantomData};

use super::{Ctx, Service, ServiceFactory};

/// Service for the `map` combinator, changing the type of a service's response.
///
/// This is created by the `ServiceExt::map` method.
pub struct Map<S, F, Res> {
    svc: S,
    f: F,
    _t: PhantomData<fn() -> Res>,
}

impl<S, F, Res> Map<S, F, Res> {
    /// Create new `Map` combinator
    pub(crate) fn new(svc: S, f: F) -> Self
    where
        S: Service,
        F: Fn(S::Res) -> Res,
    {
        Self {
            svc,
            f,
            _t: PhantomData,
        }
    }
}

impl<S, F, Res> Clone for Map<S, F, Res>
where
    S: Clone,
    F: Clone,
{
    #[inline]
    fn clone(&self) -> Self {
        Map {
            svc: self.svc.clone(),
            f: self.f.clone(),
            _t: PhantomData,
        }
    }
}

impl<S, F, Res> fmt::Debug for Map<S, F, Res>
where
    S: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Map")
            .field("svc", &self.svc)
            .field("map", &std::any::type_name::<F>())
            .finish()
    }
}

impl<S, F, Res> Service for Map<S, F, Res>
where
    S: Service,
    F: Fn(S::Res) -> Res,
{
    type St = S::St;
    type Req = S::Req;
    type Res = Res;
    type Error = S::Error;

    crate::forward_ready!(svc);
    crate::forward_poll!(svc);
    crate::forward_shutdown!(svc);

    #[inline]
    async fn call(&self, req: S::Req, ctx: Ctx<'_, Self>) -> Result<Res, S::Error> {
        ctx.call(&self.svc, req).await.map(|r| (self.f)(r))
    }
}

/// `MapNewService` new service combinator
pub struct MapFactory<Sf, F, Res> {
    sf: Sf,
    f: F,
    r: PhantomData<fn() -> Res>,
}

impl<Sf, F, Res> MapFactory<Sf, F, Res> {
    /// Create new `Map` new service instance
    pub(crate) fn new<Req>(sf: Sf, f: F) -> Self
    where
        Sf: ServiceFactory<Req>,
        F: Fn(Sf::Res) -> Res,
    {
        Self {
            sf,
            f,
            r: PhantomData,
        }
    }
}

impl<Sf, F, Res> Clone for MapFactory<Sf, F, Res>
where
    Sf: Clone,
    F: Clone,
{
    #[inline]
    fn clone(&self) -> Self {
        Self {
            sf: self.sf.clone(),
            f: self.f.clone(),
            r: PhantomData,
        }
    }
}

impl<Sf, F, Res> fmt::Debug for MapFactory<Sf, F, Res>
where
    Sf: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MapFactory")
            .field("factory", &self.sf)
            .field("map", &std::any::type_name::<F>())
            .finish()
    }
}

impl<Sf, Req, F, Res> ServiceFactory<Req> for MapFactory<Sf, F, Res>
where
    Sf: ServiceFactory<Req>,
    F: Fn(Sf::Res) -> Res + Clone,
{
    type St = Sf::St;
    type Res = Res;
    type Error = Sf::Error;

    type Service = Map<Sf::Service, F, Res>;
    type InitCfg = Sf::InitCfg;
    type InitError = Sf::InitError;

    #[inline]
    async fn create(&self, cfg: &Sf::InitCfg) -> Result<Self::Service, Self::InitError> {
        Ok(Map {
            svc: self.sf.create(cfg).await?,
            f: self.f.clone(),
            _t: PhantomData,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unused_async_trait_impl)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use crate::{Ctx, Pipeline, ReadyCtx, Service, ServiceFactory, fn_factory};

    #[derive(Debug, Default, Clone)]
    struct Srv(Rc<Cell<usize>>);

    impl Service for Srv {
        type St = ();
        type Req = ();
        type Res = ();
        type Error = ();

        async fn ready(&self, _: ReadyCtx<'_, Self>) -> Result<(), Self::Error> {
            Ok(())
        }

        async fn call(&self, _r: (), _: Ctx<'_, Self>) -> Result<(), ()> {
            Ok(())
        }

        async fn shutdown(&self) {
            self.0.set(self.0.get() + 1);
        }
    }

    #[ntex::test]
    async fn test_service() {
        let cnt_sht = Rc::new(Cell::new(0));
        let srv = Pipeline::new(Srv(cnt_sht.clone()).map(|()| "ok").clone());
        let res = srv.call((), &()).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "ok");

        let res = srv.ready(&()).await;
        assert_eq!(res, Ok(()));

        srv.shutdown().await;
        assert_eq!(cnt_sht.get(), 1);
        let _ = format!("{srv:?}");

        let cnt_sht = Rc::new(Cell::new(0));
        let svc = Srv(cnt_sht.clone()).map(|()| "ok");
        let srv = Pipeline::new(&svc);
        let res = srv.call((), &()).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "ok");

        let res = srv.ready(&()).await;
        assert_eq!(res, Ok(()));

        srv.shutdown().await;
        assert_eq!(cnt_sht.get(), 1);
        let _ = format!("{srv:?}");
    }

    #[ntex::test]
    async fn test_pipeline() {
        let srv = Pipeline::new(crate::chain(Srv::default()).map(|()| "ok").clone());
        let res = srv.call((), &()).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "ok");

        let res = srv.ready(&()).await;
        assert_eq!(res, Ok(()));
    }

    #[ntex::test]
    async fn test_factory() {
        let new_srv = fn_factory(|| async { Ok::<_, ()>(Srv::default()) })
            .map(|()| "ok")
            .clone();
        let srv = Pipeline::new(new_srv.create(&()).await.unwrap());
        let res = srv.call((), &()).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("ok"));

        let _ = format!("{new_srv:?}");
    }

    #[ntex::test]
    async fn test_pipeline_factory() {
        let new_srv =
            crate::chain_factory(fn_factory(|| async { Ok::<_, ()>(Srv::default()) }))
                .map(|()| "ok")
                .clone();
        let srv = Pipeline::new(new_srv.create(&()).await.unwrap());
        let res = srv.call((), &()).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("ok"));

        let _ = format!("{new_srv:?}");
    }
}
