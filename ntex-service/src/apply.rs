#![allow(clippy::type_complexity)]
use std::{fmt, marker};

use crate::ctx::WaitersRef;
use crate::dev::{ServiceChain, ServiceChainFactory};
use crate::{Ctx, IntoService, IntoServiceFactory, ReadyCtx, Service, ServiceFactory};

/// Apply transform function to a service.
pub fn apply_fn<S, St, Req, F, In, Out, Err, U>(
    service: U,
    f: F,
) -> ServiceChain<Apply<S, St, Req, F, In, Out, Err>, St, In>
where
    S: Service<St, Req>,
    F: AsyncFn(In, &ApplyCtx<'_, S, St>) -> Result<Out, Err>,
    U: IntoService<S, St, Req>,
    Err: From<S::Error>,
{
    crate::chain(Apply::new(service.into_service(), f))
}

/// Service factory that produces `apply_fn` service.
pub fn apply_fn_factory<Sf, St, Req, F, In, Out, Err, U>(
    service: U,
    f: F,
) -> ServiceChainFactory<ApplyFactory<Sf, St, Req, F, In, Out, Err>, St, In>
where
    Sf: ServiceFactory<St, Req>,
    F: AsyncFn(In, &ApplyCtx<'_, Sf::Service, St>) -> Result<Out, Err> + Clone,
    U: IntoServiceFactory<Sf, St, Req>,
    Err: From<Sf::Error>,
{
    crate::chain_factory(ApplyFactory::new(service.into_factory(), f))
}

#[derive(Debug)]
pub struct ApplyCtx<'a, S, St> {
    idx: u32,
    waiters: &'a WaitersRef,
    service: &'a S,
    st: &'a St,
}

impl<S, St> ApplyCtx<'_, S, St> {
    #[inline]
    /// Wait for service readiness and then call service.
    pub async fn call<Req>(&self, req: Req) -> Result<S::Res, S::Error>
    where
        S: Service<St, Req>,
    {
        Ctx::<S, St>::new(self.idx, self.waiters, self.st)
            .call(&self.service, req)
            .await
    }
}

/// `Apply` service combinator
pub struct Apply<S, St, Req, F, In, Out, Err> {
    svc: S,
    f: F,
    r: marker::PhantomData<fn() -> (St, Req, In, Out, Err)>,
}

impl<S, St, Req, F, In, Out, Err> Apply<S, St, Req, F, In, Out, Err>
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

impl<S, St, Req, F, In, Out, Err> Clone for Apply<S, St, Req, F, In, Out, Err>
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

impl<S, St, Req, F, In, Out, Err> fmt::Debug for Apply<S, St, Req, F, In, Out, Err>
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

impl<S, St, Req, F, In, Out, Err> Service<St, In> for Apply<S, St, Req, F, In, Out, Err>
where
    S: Service<St, Req>,
    F: AsyncFn(In, &ApplyCtx<'_, S, St>) -> Result<Out, Err>,
    Err: From<S::Error>,
{
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

    crate::forward_poll!(svc);
    crate::forward_shutdown!(svc);
}

/// `apply()` service factory
pub struct ApplyFactory<Sf, St, Req, F, In, Out, Err>
where
    Sf: ServiceFactory<St, Req>,
    F: AsyncFn(In, &ApplyCtx<'_, Sf::Service, St>) -> Result<Out, Err> + Clone,
{
    sf: Sf,
    f: F,
    r: marker::PhantomData<fn(St, Req) -> (In, Out)>,
}

impl<Sf, St, Req, F, In, Out, Err> ApplyFactory<Sf, St, Req, F, In, Out, Err>
where
    Sf: ServiceFactory<St, Req>,
    F: AsyncFn(In, &ApplyCtx<'_, Sf::Service, St>) -> Result<Out, Err> + Clone,
    Err: From<Sf::Error>,
{
    /// Create new `ApplyNewService` new service instance
    pub(crate) fn new(sf: Sf, f: F) -> Self {
        Self {
            f,
            sf,
            r: marker::PhantomData,
        }
    }
}

impl<Sf, St, Req, F, In, Out, Err> Clone for ApplyFactory<Sf, St, Req, F, In, Out, Err>
where
    Sf: ServiceFactory<St, Req> + Clone,
    F: AsyncFn(In, &ApplyCtx<'_, Sf::Service, St>) -> Result<Out, Err> + Clone,
    Err: From<Sf::Error>,
{
    fn clone(&self) -> Self {
        Self {
            sf: self.sf.clone(),
            f: self.f.clone(),
            r: marker::PhantomData,
        }
    }
}

impl<Sf, St, Req, F, In, Out, Err> fmt::Debug for ApplyFactory<Sf, St, Req, F, In, Out, Err>
where
    Sf: ServiceFactory<St, Req> + fmt::Debug,
    F: AsyncFn(In, &ApplyCtx<'_, Sf::Service, St>) -> Result<Out, Err> + Clone,
    Err: From<Sf::Error>,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApplyFactory")
            .field("factory", &self.sf)
            .field("map", &std::any::type_name::<F>())
            .finish()
    }
}

impl<Sf, St, Req, F, In, Out, Err> ServiceFactory<St, In>
    for ApplyFactory<Sf, St, Req, F, In, Out, Err>
where
    Sf: ServiceFactory<St, Req>,
    F: AsyncFn(In, &ApplyCtx<'_, Sf::Service, St>) -> Result<Out, Err> + Clone,
    Err: From<Sf::Error>,
{
    type Res = Out;
    type Error = Err;

    type Service = Apply<Sf::Service, St, Req, F, In, Out, Err>;
    type InitCfg = Sf::InitCfg;
    type InitError = Sf::InitError;

    #[inline]
    async fn create(&self, cfg: &Sf::InitCfg) -> Result<Self::Service, Self::InitError> {
        self.sf.create(cfg).await.map(|svc| Apply {
            svc,
            f: self.f.clone(),
            r: marker::PhantomData,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unused_async_trait_impl)]
mod tests {
    use ntex::util::lazy;
    use std::{cell::Cell, rc::Rc, task::Context};

    use super::*;
    use crate::{chain, chain_factory, fn_factory};

    #[derive(Debug, Default, Clone)]
    struct Srv(Rc<Cell<usize>>);

    impl Service<(), ()> for Srv {
        type Res = ();
        type Error = ();

        async fn call(&self, _r: (), _: Ctx<'_, Self, ()>) -> Result<(), ()> {
            Ok(())
        }

        fn poll(&self, _: &mut Context<'_>) -> Result<(), Self::Error> {
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
        let srv = chain(
            apply_fn(Srv(cnt_sht.clone()), async move |req: &'static str, svc| {
                svc.call(()).await.unwrap();
                Ok((req, ()))
            })
            .clone(),
        )
        .into_pipeline();

        assert_eq!(srv.ready(&()).await, Ok::<_, Err>(()));

        lazy(|cx| srv.poll(cx)).await.unwrap();
        assert_eq!(cnt_sht.get(), 1);

        srv.shutdown().await;
        assert_eq!(cnt_sht.get(), 2);

        let res = srv.call("srv", &()).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("srv", ()));
    }

    #[ntex::test]
    async fn test_call_chain() {
        let cnt_sht = Rc::new(Cell::new(0));
        let srv = chain(Srv(cnt_sht.clone()))
            .apply_fn(async move |req: &'static str, svc| {
                svc.call(()).await.unwrap();
                Ok((req, ()))
            })
            .clone()
            .into_pipeline();

        assert_eq!(srv.ready(&()).await, Ok::<_, Err>(()));

        srv.shutdown().await;
        assert_eq!(cnt_sht.get(), 1);

        let res = srv.call("srv", &()).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("srv", ()));
        let _ = format!("{srv:?}");
    }

    #[ntex::test]
    async fn test_create() {
        let new_srv = chain_factory(
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

        assert_eq!(srv.ready(&()).await, Ok::<_, Err>(()));

        let res = srv.call("srv", &()).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("srv", ()));
        let _ = format!("{new_srv:?}");

        assert_eq!(Err, Err::from(()));
    }

    #[ntex::test]
    async fn test_create_chain() {
        let new_srv = chain_factory(fn_factory(|| async { Ok::<_, ()>(Srv::default()) }))
            .apply_fn(async move |req: &'static str, srv| {
                srv.call(()).await.unwrap();
                Ok((req, ()))
            })
            .clone();

        let srv = new_srv.pipeline(&()).await.unwrap();

        assert_eq!(srv.ready(&()).await, Ok::<_, Err>(()));

        let res = srv.call("srv", &()).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("srv", ()));
        let _ = format!("{new_srv:?}");
    }
}
