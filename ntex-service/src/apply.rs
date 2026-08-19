use std::{fmt, marker};

use crate::ctx::{Ctx, ReadyCtx, WaitersRef};
use crate::{IntoService, IntoServiceFactory, Service, ServiceFactory};
use crate::{ServiceChain, ServiceChainFactory};

/// Apply transform function to a service.
pub fn apply_fn<S, St, F, In, Out, Err>(
    service: impl IntoService<S, St>,
    f: F,
) -> ServiceChain<Apply<S, St, F, In, Out, Err>, St>
where
    S: Service<St>,
    F: AsyncFn(In, &ApplyCtx<'_, S, St>) -> Result<Out, Err>,
    Err: From<S::Error>,
{
    crate::svc(Apply::new(service.into_service(), f))
}

/// Service factory that produces `apply_fn` service.
pub fn apply_fn_factory<Sf, St, Req, Cfg, F, In, Out, Err>(
    service: impl IntoServiceFactory<Sf, St, Req, Cfg>,
    f: F,
) -> ServiceChainFactory<ApplyFactory<F, Sf, St, Req, Cfg, In, Out, Err>, St, In, Cfg>
where
    Sf: ServiceFactory<St, Req, Cfg>,
    F: AsyncFn(In, &ApplyCtx<'_, Sf::Service, St>) -> Result<Out, Err> + Clone,
    Err: From<Sf::Error>,
{
    crate::factory_with_st(ApplyFactory::new(service.into_factory(), f))
}

#[derive(Debug)]
pub struct ApplyCtx<'a, S, St> {
    idx: u32,
    waiters: &'a WaitersRef,
    service: &'a S,
    st: &'a St,
}

impl<S: Service<St>, St> ApplyCtx<'_, S, St> {
    #[inline]
    /// Wait for service readiness and then call service.
    pub async fn call(&self, req: S::Req) -> Result<S::Res, S::Error> {
        Ctx::<S, St>::new(self.idx, self.waiters, self.st)
            .call(&self.service, req)
            .await
    }
}

/// `Apply` service combinator
pub struct Apply<S, St, F, In, Out, Err> {
    svc: S,
    f: F,
    r: marker::PhantomData<fn(St) -> (In, Out, Err)>,
}

impl<S, St, F, In, Out, Err> Apply<S, St, F, In, Out, Err>
where
    F: AsyncFn(In, &ApplyCtx<'_, S, St>) -> Result<Out, Err>,
{
    pub(crate) fn new(svc: S, f: F) -> Self {
        Apply {
            f,
            svc,
            r: marker::PhantomData,
        }
    }
}

impl<S, St, F, In, Out, Err> Clone for Apply<S, St, F, In, Out, Err>
where
    S: Clone,
    F: Clone,
{
    fn clone(&self) -> Self {
        Apply {
            svc: self.svc.clone(),
            f: self.f.clone(),
            r: marker::PhantomData,
        }
    }
}

impl<S, St, F, In, Out, Err> fmt::Debug for Apply<S, St, F, In, Out, Err>
where
    S: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Apply")
            .field("svc", &self.svc)
            .field("map", &std::any::type_name::<F>())
            .finish()
    }
}

impl<S, St, F, In, Out, Err> Service<St> for Apply<S, St, F, In, Out, Err>
where
    S: Service<St>,
    F: AsyncFn(In, &ApplyCtx<'_, S, St>) -> Result<Out, Err>,
    Err: From<S::Error>,
{
    type Req = In;
    type Res = Out;
    type Error = Err;

    #[inline]
    async fn ready(&self, ctx: ReadyCtx<'_, Self, St>) -> Result<(), Err> {
        ctx.ready(&self.svc).await.map_err(From::from)
    }

    #[inline]
    async fn call(&self, req: In, ctx: Ctx<'_, Self, St>) -> Result<Out, Err> {
        let (idx, waiters, st) = ctx.inner();

        let ctx = ApplyCtx {
            idx,
            waiters,
            st,
            service: &self.svc,
        };
        (self.f)(req, &ctx).await
    }

    crate::forward_shutdown!(svc);
}

/// `apply()` service factory
pub struct ApplyFactory<F, Sf, St, Req, Cfg, In, Out, Err>
where
    F: AsyncFn(In, &ApplyCtx<'_, Sf::Service, St>) -> Result<Out, Err> + Clone,
    Sf: ServiceFactory<St, Req, Cfg>,
{
    f: F,
    sf: Sf,
    r: marker::PhantomData<fn(St, Req, Cfg) -> (In, Out)>,
}

impl<F, Sf, St, Req, Cfg, In, Out, Err> ApplyFactory<F, Sf, St, Req, Cfg, In, Out, Err>
where
    F: AsyncFn(In, &ApplyCtx<'_, Sf::Service, St>) -> Result<Out, Err> + Clone,
    Sf: ServiceFactory<St, Req, Cfg>,
{
    /// Create new `ApplyFactory` new service instance
    pub(crate) fn new(sf: Sf, f: F) -> Self
    where
        Sf: ServiceFactory<St, Req, Cfg>,
        Err: From<Sf::Error>,
    {
        Self {
            f,
            sf,
            r: marker::PhantomData,
        }
    }
}

impl<F, Sf, St, Req, Cfg, In, Out, Err> Clone for ApplyFactory<F, Sf, St, Req, Cfg, In, Out, Err>
where
    F: AsyncFn(In, &ApplyCtx<'_, Sf::Service, St>) -> Result<Out, Err> + Clone,
    Sf: ServiceFactory<St, Req, Cfg> + Clone,
{
    fn clone(&self) -> Self {
        Self {
            f: self.f.clone(),
            sf: self.sf.clone(),
            r: marker::PhantomData,
        }
    }
}

impl<F, Sf, St, Req, Cfg, In, Out, Err> fmt::Debug
    for ApplyFactory<F, Sf, St, Req, Cfg, In, Out, Err>
where
    F: AsyncFn(In, &ApplyCtx<'_, Sf::Service, St>) -> Result<Out, Err> + Clone,
    Sf: ServiceFactory<St, Req, Cfg> + fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApplyFactory")
            .field("factory", &self.sf)
            .field("map", &std::any::type_name::<F>())
            .finish()
    }
}

impl<F, Sf, St, Req, Cfg, In, Out, Err> ServiceFactory<St, In, Cfg>
    for ApplyFactory<F, Sf, St, Req, Cfg, In, Out, Err>
where
    F: AsyncFn(In, &ApplyCtx<'_, Sf::Service, St>) -> Result<Out, Err> + Clone,
    Sf: ServiceFactory<St, Req, Cfg>,
    Err: From<Sf::Error>,
{
    type Res = Out;
    type Error = Err;

    type Service = Apply<Sf::Service, St, F, In, Out, Err>;
    type InitError = Sf::InitError;

    #[inline]
    async fn create(&self, cfg: &Cfg) -> Result<Self::Service, Self::InitError> {
        self.sf.create(cfg).await.map(|svc| Apply {
            svc,
            f: self.f.clone(),
            r: marker::PhantomData,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use super::*;
    use crate::{factory, fn_factory, svc};

    #[derive(Debug, Default, Clone)]
    struct Srv(Rc<Cell<usize>>);

    impl Service for Srv {
        type Req = ();
        type Res = ();
        type Error = ();

        async fn call(&self, _r: (), _: Ctx<'_, Self>) -> Result<(), ()> {
            Ok(())
        }

        async fn ready(&self, _: ReadyCtx<'_, Self>) -> Result<(), ()> {
            self.0.set(self.0.get() + 1);
            Ok(())
        }

        async fn shutdown(&self) {
            self.0.set(self.0.get() + 1);
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    struct Err;

    impl From<()> for Err {
        fn from(_e: ()) -> Self {
            Err
        }
    }

    #[ntex::test]
    async fn test_call() {
        let cnt_sht = Rc::new(Cell::new(0));
        let srv = svc(
            apply_fn(Srv(cnt_sht.clone()), async move |req: &'static str, svc| {
                svc.call(()).await.unwrap();
                Ok((req, ()))
            })
            .clone(),
        )
        .into_pipeline();

        assert_eq!(srv.ready().await, Ok::<_, Err>(()));

        srv.shutdown().await;
        assert_eq!(cnt_sht.get(), 2);

        let res = srv.call("srv").await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("srv", ()));
    }

    #[ntex::test]
    async fn test_call_svc() {
        let cnt_sht = Rc::new(Cell::new(0));
        let srv = svc(Srv(cnt_sht.clone()))
            .apply_fn(async move |req: &'static str, svc| {
                svc.call(()).await.unwrap();
                Ok((req, ()))
            })
            .clone()
            .into_pipeline();

        assert_eq!(srv.ready().await, Ok::<_, Err>(()));

        srv.shutdown().await;
        assert_eq!(cnt_sht.get(), 2);

        let res = srv.call("srv").await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("srv", ()));
        let _ = format!("{srv:?}");
    }

    #[ntex::test]
    async fn test_create() {
        let new_srv = factory(
            apply_fn_factory(
                fn_factory(|| async { Ok::<_, ()>(Srv::default()) }),
                async move |req: &'static str, srv| {
                    srv.call(()).await.unwrap();
                    Ok((req, ()))
                },
            )
            .clone(),
        );

        let srv = new_srv.pipeline(&()).await.unwrap();

        assert_eq!(srv.ready().await, Ok::<_, Err>(()));

        let res = srv.call("srv").await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("srv", ()));
        let _ = format!("{new_srv:?}");

        assert_eq!(Err, Err::from(()));
    }

    #[ntex::test]
    async fn test_create_chain() {
        let new_srv = factory(fn_factory(|| async { Ok::<_, ()>(Srv::default()) }))
            .apply_fn(async move |req: &'static str, srv| {
                srv.call(()).await.unwrap();
                Ok((req, ()))
            })
            .clone();

        let srv = new_srv.pipeline(&()).await.unwrap();

        assert_eq!(srv.ready().await, Ok::<_, Err>(()));

        let res = srv.call("srv").await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("srv", ()));
        let _ = format!("{new_srv:?}");
    }
}
