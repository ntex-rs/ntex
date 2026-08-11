#![allow(clippy::type_complexity)]
use std::{fmt, marker};

use crate::ctx::WaitersRef;
use crate::dev::{ServiceChain, ServiceChainFactory};
use crate::{IntoService, IntoServiceFactory, Service, ServiceCtx, ServiceFactory};

/// Apply transform function to a service.
pub fn apply_fn<T, F, In, Out, Err, U>(
    service: U,
    f: F,
) -> ServiceChain<Apply<T, F, In, Out, Err>>
where
    T: Service,
    F: AsyncFn(In, &ApplyCtx<'_, T>) -> Result<Out, Err>,
    U: IntoService<T>,
    Err: From<T::Error>,
{
    crate::chain(Apply::new(service.into_service(), f))
}

/// Service factory that produces `apply_fn` service.
pub fn apply_fn_factory<T, St, Req, F, In, Out, Err, U>(
    service: U,
    f: F,
) -> ServiceChainFactory<ApplyFactory<T, St, Req, F, In, Out, Err>, St, In>
where
    T: ServiceFactory<St, Req>,
    F: AsyncFn(In, &ApplyCtx<'_, T::Service>) -> Result<Out, Err> + Clone,
    U: IntoServiceFactory<T, St, Req>,
    Err: From<T::Error>,
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
    pub async fn call(&self, req: S::Req) -> Result<S::Res, S::Error>
    where
        S: Service,
    {
        let ctx = ServiceCtx::new(self.idx, self.waiters, self.st);

        self.service.ready(ctx).await?;
        self.service.call(req, ctx).await
    }
}

/// `Apply` service combinator
pub struct Apply<T, F, In, Out, Err> {
    service: T,
    f: F,
    r: marker::PhantomData<fn() -> (In, Out, Err)>,
}

impl<T, F, In, Out, Err> Apply<T, F, In, Out, Err>
where
    T: Service,
    F: AsyncFn(In, &ApplyCtx<'_, T>) -> Result<Out, Err>,
    Err: From<T::Error>,
{
    pub(crate) fn new(service: T, f: F) -> Self {
        Apply {
            f,
            service,
            r: marker::PhantomData,
        }
    }
}

impl<T, F, In, Out, Err> Clone for Apply<T, F, In, Out, Err>
where
    T: Clone,
    F: Clone,
{
    fn clone(&self) -> Self {
        Apply {
            service: self.service.clone(),
            f: self.f.clone(),
            r: marker::PhantomData,
        }
    }
}

impl<T, F, In, Out, Err> fmt::Debug for Apply<T, F, In, Out, Err>
where
    T: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Apply")
            .field("service", &self.service)
            .field("map", &std::any::type_name::<F>())
            .finish()
    }
}

impl<T, F, In, Out, Err> Service for Apply<T, F, In, Out, Err>
where
    T: Service,
    F: AsyncFn(In, &ApplyCtx<'_, T>) -> Result<Out, Err>,
    Err: From<T::Error>,
{
    type St = T::St;
    type Req = In;
    type Res = Out;
    type Error = Err;

    #[inline]
    async fn ready(&self, ctx: ServiceCtx<'_, Self>) -> Result<(), Err> {
        ctx.ready(&self.service).await.map_err(From::from)
    }

    #[inline]
    async fn call(
        &self,
        req: In,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<Self::Res, Self::Error> {
        let (idx, waiters, st) = ctx.inner();

        let ctx = ApplyCtx {
            idx,
            waiters,
            st,
            service: &self.service,
        };
        (self.f)(req, &ctx).await
    }

    crate::forward_poll!(service);
    crate::forward_shutdown!(service);
}

/// `apply()` service factory
pub struct ApplyFactory<T, St, Req, F, In, Out, Err>
where
    T: ServiceFactory<St, Req>,
    F: AsyncFn(In, &ApplyCtx<'_, T::Service>) -> Result<Out, Err> + Clone,
{
    service: T,
    f: F,
    r: marker::PhantomData<fn(St, Req) -> (In, Out)>,
}

impl<T, St, Req, F, In, Out, Err> ApplyFactory<T, St, Req, F, In, Out, Err>
where
    T: ServiceFactory<St, Req>,
    F: AsyncFn(In, &ApplyCtx<'_, T::Service>) -> Result<Out, Err> + Clone,
    Err: From<T::Error>,
{
    /// Create new `ApplyNewService` new service instance
    pub(crate) fn new(service: T, f: F) -> Self {
        Self {
            f,
            service,
            r: marker::PhantomData,
        }
    }
}

impl<T, St, Req, F, In, Out, Err> Clone for ApplyFactory<T, St, Req, F, In, Out, Err>
where
    T: ServiceFactory<St, Req> + Clone,
    F: AsyncFn(In, &ApplyCtx<'_, T::Service>) -> Result<Out, Err> + Clone,
    Err: From<T::Error>,
{
    fn clone(&self) -> Self {
        Self {
            service: self.service.clone(),
            f: self.f.clone(),
            r: marker::PhantomData,
        }
    }
}

impl<T, St, Req, F, In, Out, Err> fmt::Debug for ApplyFactory<T, St, Req, F, In, Out, Err>
where
    T: ServiceFactory<St, Req> + fmt::Debug,
    F: AsyncFn(In, &ApplyCtx<'_, T::Service>) -> Result<Out, Err> + Clone,
    Err: From<T::Error>,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApplyFactory")
            .field("factory", &self.service)
            .field("map", &std::any::type_name::<F>())
            .finish()
    }
}

impl<T, St, Req, F, In, Out, Err> ServiceFactory<St, In>
    for ApplyFactory<T, St, Req, F, In, Out, Err>
where
    T: ServiceFactory<St, Req>,
    F: AsyncFn(In, &ApplyCtx<'_, T::Service>) -> Result<Out, Err> + Clone,
    Err: From<T::Error>,
{
    type Res = Out;
    type Error = Err;

    type Service = Apply<T::Service, F, In, Out, Err>;
    type InitCfg = T::InitCfg;
    type InitError = T::InitError;

    #[inline]
    async fn create(&self, cfg: &T::InitCfg) -> Result<Self::Service, Self::InitError> {
        self.service.create(cfg).await.map(|service| Apply {
            service,
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

    impl Service for Srv {
        type St = ();
        type Req = ();
        type Res = ();
        type Error = ();

        async fn call(&self, _r: (), _: ServiceCtx<'_, Self>) -> Result<(), ()> {
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
