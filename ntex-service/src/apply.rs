#![allow(clippy::type_complexity)]
use std::{fmt, marker};

use crate::ctx::WaitersRef;
use crate::dev::{ServiceChain, ServiceChainFactory};
use crate::svc_fct::{ErrorOf, ServiceOf};
use crate::{IntoService, IntoServiceFactory, Service, ServiceCtx, ServiceFactory};

/// Apply transform function to a service.
pub fn apply_fn<T, Req, F, In, Out, Err, U>(
    service: U,
    f: F,
) -> ServiceChain<Apply<T, Req, F, In, Out, Err>, In>
where
    T: Service<Req>,
    F: AsyncFn(In, &ApplyCtx<'_, T, T::Data>) -> Result<Out, Err>,
    U: IntoService<T, Req>,
    Err: From<T::Error>,
{
    crate::chain(Apply::new(service.into_service(), f))
}

/// Service factory that produces `apply_fn` service.
pub fn apply_fn_factory<T, Req, Cfg, F, In, Out, Err, U>(
    service: U,
    f: F,
) -> ServiceChainFactory<ApplyFactory<T, Req, Cfg, F, In, Out, Err>, In, Cfg>
where
    T: ServiceFactory<Req, Cfg>,
    T::Response: Service<Req, Data = T::Data>,
    T::Data: Clone,
    F: AsyncFn(In, &ApplyCtx<'_, ServiceOf<T, Cfg>, T::Data>) -> Result<Out, Err> + Clone,
    U: IntoServiceFactory<T, Req, Cfg>,
    Err: From<ErrorOf<T, Req, Cfg>>,
{
    crate::chain_factory(ApplyFactory::new(service.into_factory(), f))
}

#[derive(Debug)]
pub struct ApplyCtx<'a, S, D = ()> {
    idx: u32,
    waiters: &'a WaitersRef,
    service: &'a S,
    data: &'a D,
}

impl<'a, S, D> ApplyCtx<'a, S, D> {
    #[inline]
    /// Wait for service readiness and then call service.
    pub async fn call<R>(&self, req: R) -> Result<S::Response, S::Error>
    where
        S: Service<R, Data = D>,
        R: 'a,
    {
        let ctx = ServiceCtx::new(self.idx, self.waiters);

        ctx.ready(self.service, self.data).await?;
        self.service.call(req, self.data, ctx).await
    }
}

/// `Apply` service combinator
pub struct Apply<T, Req, F, In, Out, Err> {
    service: T,
    f: F,
    r: marker::PhantomData<fn(Req) -> (In, Out, Err)>,
}

impl<T, Req, F, In, Out, Err> Apply<T, Req, F, In, Out, Err>
where
    T: Service<Req>,
    F: AsyncFn(In, &ApplyCtx<'_, T, T::Data>) -> Result<Out, Err>,
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

impl<T, Req, F, In, Out, Err> Clone for Apply<T, Req, F, In, Out, Err>
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

impl<T, Req, F, In, Out, Err> fmt::Debug for Apply<T, Req, F, In, Out, Err>
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

impl<T, Req, F, In, Out, Err> Service<In> for Apply<T, Req, F, In, Out, Err>
where
    T: Service<Req>,
    F: AsyncFn(In, &ApplyCtx<'_, T, T::Data>) -> Result<Out, Err>,
    Err: From<T::Error>,
{
    type Response = Out;
    type Error = Err;
    type Data = T::Data;

    #[inline]
    async fn ready(&self, data: &Self::Data, ctx: ServiceCtx<'_, Self>) -> Result<(), Err> {
        ctx.ready(&self.service, data).await.map_err(From::from)
    }

    #[inline]
    async fn call(
        &self,
        req: In,
        data: &Self::Data,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        let (idx, waiters) = ctx.inner();

        let ctx = ApplyCtx {
            idx,
            waiters,
            service: &self.service,
            data,
        };
        (self.f)(req, &ctx).await
    }

    crate::forward_poll!(service);
    crate::forward_shutdown!(service);
}

/// `apply()` service factory
pub struct ApplyFactory<T, Req, Cfg, F, In, Out, Err>
where
    T: ServiceFactory<Req, Cfg>,
    T::Response: Service<Req, Data = T::Data>,
    F: AsyncFn(In, &ApplyCtx<'_, ServiceOf<T, Cfg>, T::Data>) -> Result<Out, Err> + Clone,
{
    service: T,
    f: F,
    r: marker::PhantomData<fn(Req, Cfg) -> (In, Out)>,
}

impl<T, Req, Cfg, F, In, Out, Err> ApplyFactory<T, Req, Cfg, F, In, Out, Err>
where
    T: ServiceFactory<Req, Cfg>,
    T::Response: Service<Req, Data = T::Data>,
    F: AsyncFn(In, &ApplyCtx<'_, ServiceOf<T, Cfg>, T::Data>) -> Result<Out, Err> + Clone,
    Err: From<ErrorOf<T, Req, Cfg>>,
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

impl<T, Req, Cfg, F, In, Out, Err> Clone for ApplyFactory<T, Req, Cfg, F, In, Out, Err>
where
    T: ServiceFactory<Req, Cfg> + Clone,
    T::Response: Service<Req, Data = T::Data>,
    F: AsyncFn(In, &ApplyCtx<'_, ServiceOf<T, Cfg>, T::Data>) -> Result<Out, Err> + Clone,
    Err: From<ErrorOf<T, Req, Cfg>>,
{
    fn clone(&self) -> Self {
        Self {
            service: self.service.clone(),
            f: self.f.clone(),
            r: marker::PhantomData,
        }
    }
}

impl<T, Req, Cfg, F, In, Out, Err> fmt::Debug for ApplyFactory<T, Req, Cfg, F, In, Out, Err>
where
    T: ServiceFactory<Req, Cfg> + fmt::Debug,
    T::Response: Service<Req, Data = T::Data>,
    F: AsyncFn(In, &ApplyCtx<'_, ServiceOf<T, Cfg>, T::Data>) -> Result<Out, Err> + Clone,
    Err: From<ErrorOf<T, Req, Cfg>>,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApplyFactory")
            .field("factory", &self.service)
            .field("map", &std::any::type_name::<F>())
            .finish()
    }
}

impl<T, Req, Cfg, F, In, Out, Err> Service<Cfg>
    for ApplyFactory<T, Req, Cfg, F, In, Out, Err>
where
    T: ServiceFactory<Req, Cfg>,
    T::Response: Service<Req, Data = T::Data>,
    F: AsyncFn(In, &ApplyCtx<'_, ServiceOf<T, Cfg>, T::Data>) -> Result<Out, Err> + Clone,
    Err: From<ErrorOf<T, Req, Cfg>>,
{
    type Response = Apply<ServiceOf<T, Cfg>, Req, F, In, Out, Err>;
    type Error = T::Error;
    type Data = T::Data;

    #[inline]
    async fn call(
        &self,
        cfg: Cfg,
        data: &Self::Data,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        ctx.call(&self.service, cfg, data)
            .await
            .map(|service| Apply {
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

    impl Service<()> for Srv {
        type Response = ();
        type Error = ();
        type Data = ();

        async fn call(
            &self,
            _r: (),
            _: &Self::Data,
            _: ServiceCtx<'_, Self>,
        ) -> Result<(), ()> {
            Ok(())
        }

        fn poll(&self, _: &Self::Data, _: &mut Context<'_>) -> Result<(), Self::Error> {
            self.0.set(self.0.get() + 1);
            Ok(())
        }

        async fn shutdown(&self, _: &Self::Data) {
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
        .into_pipeline(());

        assert_eq!(srv.ready().await, Ok::<_, Err>(()));

        lazy(|cx| srv.poll(cx)).await.unwrap();
        assert_eq!(cnt_sht.get(), 1);

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
            .into_pipeline(());

        assert_eq!(srv.ready().await, Ok::<_, Err>(()));

        srv.shutdown().await;
        assert_eq!(cnt_sht.get(), 1);

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

        let srv = new_srv.pipeline(&(), &()).await.unwrap();

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

        let srv = new_srv.pipeline(&(), &()).await.unwrap();

        assert_eq!(srv.ready().await, Ok::<_, Err>(()));

        let res = srv.call("srv").await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("srv", ()));
        let _ = format!("{new_srv:?}");
    }
}
