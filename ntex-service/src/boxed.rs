#![allow(clippy::type_complexity)]
use std::{fmt, future::Future, pin::Pin, task::Context};

use crate::ctx::{ServiceCtx, WaitersRef};

type BoxFuture<'a, I, E> = Pin<Box<dyn Future<Output = Result<I, E>> + 'a>>;
pub struct BoxService<Req, St, Res, Err>(
    Box<dyn ServiceObj<St = St, Req = Req, Res = Res, Err = Err>>,
);
pub struct BoxServiceFactory<Cfg, Req, St, Res, Err, InitError>(
    Box<
        dyn ServiceFactoryObj<
                Req,
                Cfg,
                St = St,
                Res = Res,
                Err = Err,
                InitError = InitError,
            >,
    >,
);

/// Creates a boxed service factory.
pub fn factory<F, R, C>(
    factory: F,
) -> BoxServiceFactory<C, R, F::St, F::Res, F::Err, F::InitError>
where
    R: 'static,
    C: 'static,
    F: crate::ServiceFactory<R, C> + 'static,
    F::Service: 'static,
{
    BoxServiceFactory(Box::new(factory))
}

/// Creates a boxed service.
pub fn service<S>(service: S) -> BoxService<S::Req, S::St, S::Res, S::Err>
where
    S: crate::Service + 'static,
{
    BoxService(Box::new(service))
}

impl<Req, St, Res, Err> fmt::Debug for BoxService<Req, St, Res, Err> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BoxService").finish()
    }
}

impl<Cfg, Req, St, Res, Err, InitError> fmt::Debug
    for BoxServiceFactory<Cfg, Req, St, Res, Err, InitError>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BoxServiceFactory").finish()
    }
}

trait ServiceObj {
    type St;
    type Req;
    type Res;
    type Err;

    fn ready<'a>(
        &'a self,
        idx: u32,
        waiters: &'a WaitersRef,
        st: &'a Self::St,
    ) -> BoxFuture<'a, (), Self::Err>;

    fn call<'a>(
        &'a self,
        req: Self::Req,
        idx: u32,
        waiters: &'a WaitersRef,
        st: &'a Self::St,
    ) -> BoxFuture<'a, Self::Res, Self::Err>;

    fn shutdown<'a>(&'a self) -> Pin<Box<dyn Future<Output = ()> + 'a>>;

    fn poll(&self, cx: &mut Context<'_>) -> Result<(), Self::Err>;
}

impl<S> ServiceObj for S
where
    S: crate::Service,
{
    type St = S::St;
    type Req = S::Req;
    type Res = S::Res;
    type Err = S::Err;

    #[inline]
    fn ready<'a>(
        &'a self,
        idx: u32,
        waiters: &'a WaitersRef,
        st: &'a S::St,
    ) -> BoxFuture<'a, (), Self::Err> {
        Box::pin(
            async move { ServiceCtx::<'a, S>::new(idx, waiters, st).ready(self).await },
        )
    }

    #[inline]
    fn shutdown<'a>(&'a self) -> Pin<Box<dyn Future<Output = ()> + 'a>> {
        Box::pin(crate::Service::shutdown(self))
    }

    #[inline]
    fn call<'a>(
        &'a self,
        req: S::Req,
        idx: u32,
        waiters: &'a WaitersRef,
        st: &'a S::St,
    ) -> BoxFuture<'a, Self::Res, Self::Err> {
        Box::pin(async move {
            ServiceCtx::<'a, S>::new(idx, waiters, st)
                .call_nowait(self, req)
                .await
        })
    }

    #[inline]
    fn poll(&self, cx: &mut Context<'_>) -> Result<(), Self::Err> {
        crate::Service::poll(self, cx)
    }
}

trait ServiceFactoryObj<Req, Cfg> {
    type St;
    type Res;
    type Err;
    type InitError;

    fn create<'a>(
        &'a self,
        cfg: Cfg,
    ) -> BoxFuture<'a, BoxService<Req, Self::St, Self::Res, Self::Err>, Self::InitError>
    where
        Cfg: 'a;
}

impl<F, Req, Cfg> ServiceFactoryObj<Req, Cfg> for F
where
    Cfg: 'static,
    Req: 'static,
    F: crate::ServiceFactory<Req, Cfg>,
    F::Service: 'static,
{
    type St = F::St;
    type Res = F::Res;
    type Err = F::Err;
    type InitError = F::InitError;

    #[inline]
    fn create<'a>(
        &'a self,
        cfg: Cfg,
    ) -> BoxFuture<'a, BoxService<Req, Self::St, Self::Res, Self::Err>, Self::InitError>
    where
        Cfg: 'a,
    {
        let fut = crate::ServiceFactory::create(self, cfg);
        Box::pin(async move { fut.await.map(service) })
    }
}

impl<Req, St, Res, Err> crate::Service for BoxService<Req, St, Res, Err>
where
    Req: 'static,
{
    type St = St;
    type Req = Req;
    type Res = Res;
    type Err = Err;

    #[inline]
    async fn ready(&self, ctx: ServiceCtx<'_, Self>) -> Result<(), Self::Err> {
        let (idx, waiters, _) = ctx.inner();
        self.0.ready(idx, waiters, ctx.st()).await
    }

    #[inline]
    async fn shutdown(&self) {
        self.0.shutdown().await;
    }

    #[inline]
    async fn call(&self, req: Req, ctx: ServiceCtx<'_, Self>) -> Result<Res, Err> {
        let (idx, waiters, st) = ctx.inner();
        self.0.call(req, idx, waiters, st).await
    }

    #[inline]
    fn poll(&self, cx: &mut Context<'_>) -> Result<(), Self::Err> {
        self.0.poll(cx)
    }
}

impl<C, Req, St, Res, Err, InitError> crate::ServiceFactory<Req, C>
    for BoxServiceFactory<C, Req, St, Res, Err, InitError>
where
    Req: 'static,
{
    type St = St;
    type Res = Res;
    type Err = Err;

    type Service = BoxService<Req, St, Res, Err>;
    type InitError = InitError;

    #[inline]
    async fn create(&self, cfg: C) -> Result<Self::Service, Self::InitError> {
        self.0.create(cfg).await
    }
}
