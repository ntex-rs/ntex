#![allow(clippy::type_complexity)]
use std::{fmt, marker};

use crate::ctx::WaitersRef;
use crate::dev::{ServiceChain, ServiceChainFactory};
use crate::{Ctx, IntoService, IntoServiceFactory, ReadyCtx, Service, ServiceFactory};

/// Apply transform function to a service.
pub fn apply_fn<S, F, In, Out, Err, U>(
    service: U,
    f: F,
) -> ServiceChain<Apply<S, F, In, Out, Err>>
where
    S: Service,
    F: AsyncFn(In, &ApplyCtx<'_, S>) -> Result<Out, Err>,
    U: IntoService<S>,
    Err: From<S::Error>,
{
    crate::chain(Apply::new(service.into_service(), f))
}

/// Service factory that produces `apply_fn` service.
pub fn apply_fn_factory<Sf, Req, F, In, Out, Err, U>(
    service: U,
    f: F,
) -> ServiceChainFactory<ApplyFactory<Sf, Req, F, In, Out, Err>, In>
where
    Sf: ServiceFactory<Req>,
    F: AsyncFn(In, &ApplyCtx<'_, Sf::Service>) -> Result<Out, Err> + Clone,
    U: IntoServiceFactory<Sf, Req>,
    Err: From<Sf::Error>,
{
    crate::chain_factory(ApplyFactory::new(service.into_factory(), f))
}

#[derive(Debug)]
pub struct ApplyCtx<'a, S: Service> {
    idx: u32,
    waiters: &'a WaitersRef,
    service: &'a S,
    st: &'a S::St,
}

impl<S: Service> ApplyCtx<'_, S> {
    #[inline]
    /// Wait for service readiness and then call service.
    pub async fn call(&self, req: S::Req) -> Result<S::Res, S::Error> {
        Ctx::<S>::new(self.idx, self.waiters, self.st)
            .call(&self.service, req)
            .await
    }
}

/// `Apply` service combinator
pub struct Apply<S, F, In, Out, Err> {
    svc: S,
    f: F,
    r: marker::PhantomData<fn() -> (In, Out, Err)>,
}

impl<S, F, In, Out, Err> Apply<S, F, In, Out, Err>
where
    F: AsyncFn(In, &ApplyCtx<'_, S>) -> Result<Out, Err>,
{
    pub(crate) fn new(svc: S, f: F) -> Self {
        Apply {
            f,
            svc,
            r: marker::PhantomData,
        }
    }
}

impl<S, F, In, Out, Err> Clone for Apply<S, F, In, Out, Err>
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

impl<S, F, In, Out, Err> fmt::Debug for Apply<S, F, In, Out, Err>
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

impl<S, F, In, Out, Err> Service for Apply<S, F, In, Out, Err>
where
    S: Service,
    F: AsyncFn(In, &ApplyCtx<'_, S>) -> Result<Out, Err>,
    Err: From<S::Error>,
{
    type St = S::St;
    type Req = In;
    type Res = Out;
    type Error = Err;

    #[inline]
    async fn ready(&self, ctx: ReadyCtx<'_, Self>) -> Result<(), Err> {
        ctx.ready(&self.svc).await.map_err(From::from)
    }

    #[inline]
    async fn call(&self, req: In, ctx: Ctx<'_, Self>) -> Result<Out, Err> {
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
pub struct ApplyFactory<Sf, Req, F, In, Out, Err>
where
    Sf: ServiceFactory<Req>,
    F: AsyncFn(In, &ApplyCtx<'_, Sf::Service>) -> Result<Out, Err> + Clone,
{
    sf: Sf,
    f: F,
    r: marker::PhantomData<fn(Req) -> (In, Out)>,
}

impl<Sf, Req, F, In, Out, Err> ApplyFactory<Sf, Req, F, In, Out, Err>
where
    Sf: ServiceFactory<Req>,
    F: AsyncFn(In, &ApplyCtx<'_, Sf::Service>) -> Result<Out, Err> + Clone,
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

impl<Sf, Req, F, In, Out, Err> Clone for ApplyFactory<Sf, Req, F, In, Out, Err>
where
    Sf: ServiceFactory<Req> + Clone,
    F: AsyncFn(In, &ApplyCtx<'_, Sf::Service>) -> Result<Out, Err> + Clone,
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

impl<Sf, Req, F, In, Out, Err> fmt::Debug for ApplyFactory<Sf, Req, F, In, Out, Err>
where
    Sf: ServiceFactory<Req> + fmt::Debug,
    F: AsyncFn(In, &ApplyCtx<'_, Sf::Service>) -> Result<Out, Err> + Clone,
    Err: From<Sf::Error>,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApplyFactory")
            .field("factory", &self.sf)
            .field("map", &std::any::type_name::<F>())
            .finish()
    }
}

impl<Sf, Req, F, In, Out, Err> ServiceFactory<In> for ApplyFactory<Sf, Req, F, In, Out, Err>
where
    Sf: ServiceFactory<Req>,
    F: AsyncFn(In, &ApplyCtx<'_, Sf::Service>) -> Result<Out, Err> + Clone,
    Err: From<Sf::Error>,
{
    type St = Sf::St;
    type Res = Out;
    type Error = Err;

    type Service = Apply<Sf::Service, F, In, Out, Err>;
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
    use std::{cell::Cell, rc::Rc};

    use super::*;
    use crate::{chain, chain_factory, fn_factory};

    #[derive(Debug, Default, Clone)]
    struct Srv(Rc<Cell<usize>>);

    impl Service for Srv {
        type St = ();
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
        let srv = chain(
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
    async fn test_call_chain() {
        let cnt_sht = Rc::new(Cell::new(0));
        let srv = chain(Srv(cnt_sht.clone()))
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

        assert_eq!(srv.ready().await, Ok::<_, Err>(()));

        let res = srv.call("srv").await;
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

        assert_eq!(srv.ready().await, Ok::<_, Err>(()));

        let res = srv.call("srv").await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("srv", ()));
        let _ = format!("{new_srv:?}");
    }
}
