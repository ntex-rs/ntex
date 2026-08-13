use std::{fmt, marker::PhantomData};

use super::{Ctx, Service, ServiceFactory};

/// Service for the `map` combinator, changing the type of a service's response.
///
/// This is created by the `ServiceExt::map` method.
pub struct Map<A, F, Res> {
    service: A,
    f: F,
    _t: PhantomData<fn() -> Res>,
}

impl<A, F, Res> Map<A, F, Res> {
    /// Create new `Map` combinator
    pub(crate) fn new<St>(service: A, f: F) -> Self
    where
        A: Service<St>,
        F: Fn(A::Res) -> Res,
    {
        Self {
            service,
            f,
            _t: PhantomData,
        }
    }
}

impl<A, F, Res> Clone for Map<A, F, Res>
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

impl<A, F, Res> fmt::Debug for Map<A, F, Res>
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

impl<A, F, St, Res> Service<St> for Map<A, F, Res>
where
    A: Service<St>,
    F: Fn(A::Res) -> Res,
{
    type Req = A::Req;
    type Res = Res;
    type Error = A::Error;

    crate::forward_ready!(St, service);
    crate::forward_poll!(service);
    crate::forward_shutdown!(service);

    #[inline]
    async fn call(&self, req: A::Req, ctx: Ctx<'_, Self, St>) -> Result<Res, A::Error> {
        ctx.call(&self.service, req).await.map(|r| (self.f)(r))
    }
}

/// `MapNewService` new service combinator
pub struct MapFactory<A, F, St, Res> {
    a: A,
    f: F,
    r: PhantomData<fn(St) -> Res>,
}

impl<A, F, St, Res> MapFactory<A, F, St, Res>
where
    A: ServiceFactory<St>,
    F: Fn(A::Res) -> Res,
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

impl<A, F, St, Res> Clone for MapFactory<A, F, St, Res>
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

impl<A, F, St, Res> fmt::Debug for MapFactory<A, F, St, Res>
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

impl<A, F, St, Res> ServiceFactory<St> for MapFactory<A, F, St, Res>
where
    A: ServiceFactory<St>,
    F: Fn(A::Res) -> Res + Clone,
{
    type Req = A::Req;
    type Res = Res;
    type Error = A::Error;

    type Service = Map<A::Service, F, Res>;
    type InitCfg = A::InitCfg;
    type InitError = A::InitError;

    #[inline]
    async fn create(&self, cfg: &A::InitCfg) -> Result<Self::Service, Self::InitError> {
        Ok(Map {
            service: self.a.create(cfg).await?,
            f: self.f.clone(),
            _t: PhantomData,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unused_async_trait_impl)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use crate::{Ctx, Pipeline, Service, ServiceFactory, fn_factory};

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
