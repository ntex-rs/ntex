use std::{fmt, marker::PhantomData};

use super::{Service, ServiceCtx, ServiceFactory};
use crate::svc_fct::{ResponseOf, ServiceOf};

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

impl<S, F, Req, Res> Service<Req> for Map<S, F, Req, Res>
where
    S: Service<Req>,
    F: Fn(S::Response) -> Res,
{
    type Response = Res;
    type Error = S::Error;
    type Data = S::Data;

    crate::forward_ready!(service);
    crate::forward_poll!(service);
    crate::forward_shutdown!(service);

    #[inline]
    async fn call(
        &self,
        req: Req,
        data: &Self::Data,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        ctx.call(&self.service, req, data)
            .await
            .map(|r| (self.f)(r))
    }
}

/// `MapNewService` new service combinator
pub struct MapFactory<S, F, Req, Res, Cfg> {
    s: S,
    f: F,
    r: PhantomData<fn(Req, Cfg) -> Res>,
}

impl<S, F, Req, Res, Cfg> MapFactory<S, F, Req, Res, Cfg>
where
    S: ServiceFactory<Req, Cfg>,
    F: Fn(ResponseOf<S, Req, Cfg>) -> Res,
{
    /// Create new `Map` new service instance
    pub(crate) fn new(s: S, f: F) -> Self {
        Self {
            s,
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
            s: self.s.clone(),
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
            .field("factory", &self.s)
            .field("map", &std::any::type_name::<F>())
            .finish()
    }
}

impl<S, F, Req, Res, Cfg> Service<Cfg> for MapFactory<S, F, Req, Res, Cfg>
where
    S: ServiceFactory<Req, Cfg>,
    S::Response: Service<Req, Data = S::Data>,
    S::Data: Clone,
    F: Fn(ResponseOf<S, Req, Cfg>) -> Res + Clone,
{
    type Response = Map<ServiceOf<S, Cfg>, F, Req, Res>;
    type Error = S::Error;
    type Data = S::Data;

    #[inline]
    async fn call(
        &self,
        cfg: Cfg,
        data: &Self::Data,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        Ok(Map {
            service: ctx.call(&self.s, cfg, data).await?,
            f: self.f.clone(),
            _t: PhantomData,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unused_async_trait_impl)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use crate::{Pipeline, Service, ServiceCtx, fn_factory};

    #[derive(Debug, Default, Clone)]
    struct Srv(Rc<Cell<usize>>);

    impl Service<()> for Srv {
        type Response = ();
        type Error = ();
        type Data = ();

        async fn ready(
            &self,
            _: &Self::Data,
            _: ServiceCtx<'_, Self>,
        ) -> Result<(), Self::Error> {
            Ok(())
        }

        async fn call(
            &self,
            _r: (),
            _: &Self::Data,
            _: ServiceCtx<'_, Self>,
        ) -> Result<(), ()> {
            Ok(())
        }

        async fn shutdown(&self, _: &Self::Data) {
            self.0.set(self.0.get() + 1);
        }
    }

    #[ntex::test]
    async fn test_service() {
        let cnt_sht = Rc::new(Cell::new(0));
        let srv = Pipeline::new(Srv(cnt_sht.clone()).map(|()| "ok").clone(), ());
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
        let srv = Pipeline::new(&svc, ());
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
        let srv = Pipeline::new(crate::chain(Srv::default()).map(|()| "ok").clone(), ());
        let res = srv.call(()).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "ok");

        let res = srv.ready().await;
        assert_eq!(res, Ok(()));
    }

    #[ntex::test]
    async fn test_factory() {
        let new_srv =
            crate::chain_factory(fn_factory(|| async { Ok::<_, ()>(Srv::default()) }))
                .map(|()| "ok")
                .clone();
        let srv = new_srv.pipeline(&(), &()).await.unwrap();
        let res = srv.call(()).await;
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
        let srv = new_srv.pipeline(&(), &()).await.unwrap();
        let res = srv.call(()).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("ok"));

        let _ = format!("{new_srv:?}");
    }
}
