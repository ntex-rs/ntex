use std::{fmt, marker::PhantomData};

use super::{Ctx, Service, ServiceFactory};

/// Service for the `map` combinator, changing the type of a service's response.
///
/// This is created by the `ServiceExt::map` method.
pub struct Map<F, S, Res> {
    f: F,
    svc: S,
    _t: PhantomData<fn() -> Res>,
}

impl<F, S, Res> Map<F, S, Res> {
    /// Create new `Map` combinator
    pub(crate) fn new<St, Req>(f: F, svc: S) -> Self
    where
        F: Fn(S::Res) -> Res,
        S: Service<St, Req>,
    {
        Self {
            f,
            svc,
            _t: PhantomData,
        }
    }
}

impl<F, S, Res> Clone for Map<F, S, Res>
where
    F: Clone,
    S: Clone,
{
    #[inline]
    fn clone(&self) -> Self {
        Map {
            f: self.f.clone(),
            svc: self.svc.clone(),
            _t: PhantomData,
        }
    }
}

impl<F, S, Res> fmt::Debug for Map<F, S, Res>
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

impl<F, S, St, Req, Res> Service<St, Req> for Map<F, S, Res>
where
    S: Service<St, Req>,
    F: Fn(S::Res) -> Res,
{
    type Res = Res;
    type Error = S::Error;

    #[inline]
    async fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> Result<Res, S::Error> {
        ctx.call(&self.svc, req).await.map(|r| (self.f)(r))
    }

    crate::forward_ready!(St, svc);
    crate::forward_shutdown!(St, svc);
}

/// `MapNewService` new service combinator
pub struct MapFactory<F, Sf, Res> {
    f: F,
    sf: Sf,
    r: PhantomData<fn() -> Res>,
}

impl<F, Sf, Res> MapFactory<F, Sf, Res> {
    /// Create new `Map` new service instance
    pub(crate) fn new<St, Req, Cfg>(f: F, sf: Sf) -> Self
    where
        F: Fn(Sf::Res) -> Res,
        Sf: ServiceFactory<St, Req, Cfg>,
    {
        Self {
            f,
            sf,
            r: PhantomData,
        }
    }
}

impl<F, Sf, Res> Clone for MapFactory<F, Sf, Res>
where
    F: Clone,
    Sf: Clone,
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

impl<F, Sf, Res> fmt::Debug for MapFactory<F, Sf, Res>
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

impl<F, Sf, St, Req, Cfg, Res> ServiceFactory<St, Req, Cfg> for MapFactory<F, Sf, Res>
where
    F: Fn(Sf::Res) -> Res + Clone,
    Sf: ServiceFactory<St, Req, Cfg>,
{
    type Res = Res;
    type Error = Sf::Error;

    type Service = Map<F, Sf::Service, Res>;
    type InitError = Sf::InitError;

    #[inline]
    async fn create(&self, cfg: &Cfg) -> Result<Self::Service, Self::InitError> {
        Ok(Map {
            svc: self.sf.create(cfg).await?,
            f: self.f.clone(),
            _t: PhantomData,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use crate::{Ctx, CtxShutdown, Pipeline, Service, ServiceFactory, fn_factory};

    #[derive(Debug, Default, Clone)]
    struct Srv(Rc<Cell<usize>>);

    impl Service<(), ()> for Srv {
        type Res = ();
        type Error = ();

        async fn ready(&self, _: Ctx<'_, Self>) -> Result<(), Self::Error> {
            Ok(())
        }

        async fn call(&self, _r: (), _: Ctx<'_, Self>) -> Result<(), ()> {
            Ok(())
        }

        async fn shutdown(&self, _: CtxShutdown<'_, ()>) {
            self.0.set(self.0.get() + 1);
        }
    }

    #[ntex::test]
    async fn test_service() {
        let cnt_sht = Rc::new(Cell::new(0));
        let srv = Pipeline::new(Srv(cnt_sht.clone()).map(|()| "ok").clone());
        let res = srv.call(()).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "ok");

        let res = srv.ready().await;
        assert_eq!(res, Ok(()));

        srv.shutdown().await;
        assert_eq!(cnt_sht.get(), 1);
        let _ = format!("{srv:?}");

        let cnt_sht = Rc::new(Cell::new(0));
        let svc = Srv(cnt_sht.clone()).map(|()| "ok");
        let srv = Pipeline::new(svc);
        let res = srv.call(()).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "ok");

        let res = srv.ready().await;
        assert_eq!(res, Ok(()));

        srv.shutdown().await;
        assert_eq!(cnt_sht.get(), 1);
        let _ = format!("{srv:?}");
    }

    #[ntex::test]
    async fn test_pipeline() {
        let srv = Pipeline::new(crate::svc(Srv::default()).map(|()| "ok").clone());
        let res = srv.call(()).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "ok");

        let res = srv.ready().await;
        assert_eq!(res, Ok(()));
    }

    #[ntex::test]
    async fn test_factory() {
        let new_srv = fn_factory(|| async { Ok::<_, ()>(Srv::default()) })
            .map(|()| "ok")
            .clone();
        let srv = Pipeline::new(new_srv.create(&()).await.unwrap());
        let res = srv.call(()).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("ok"));

        let _ = format!("{new_srv:?}");
    }

    #[ntex::test]
    async fn test_pipeline_factory() {
        let new_srv = crate::factory(fn_factory(|| async { Ok::<_, ()>(Srv::default()) }))
            .map(|()| "ok")
            .clone();
        let srv = Pipeline::new(new_srv.create(&()).await.unwrap());
        let res = srv.call(()).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("ok"));

        let _ = format!("{new_srv:?}");
    }
}
