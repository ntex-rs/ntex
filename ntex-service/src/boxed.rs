#![allow(clippy::type_complexity)]
use std::{fmt, future::Future, pin::Pin, task::Context};

use crate::ctx::{Ctx, ReadyCtx, WaitersRef};

type BoxFuture<'a, I, E> = Pin<Box<dyn Future<Output = Result<I, E>> + 'a>>;
pub struct BoxService<St, Req, Res, Err>(
    Box<dyn ServiceObj<St, Req = Req, Res = Res, Error = Err>>,
);
pub struct BoxServiceFactory<St, Req, Res, Err, Cfg, InitError>(
    Box<
        dyn ServiceFactoryObj<
                St,
                Cfg,
                Req = Req,
                Res = Res,
                Error = Err,
                InitError = InitError,
            >,
    >,
);

/// Creates a boxed service factory.
pub fn factory<F, St>(
    factory: F,
) -> BoxServiceFactory<St, F::Req, F::Res, F::Error, F::InitCfg, F::InitError>
where
    St: 'static,
    F: crate::ServiceFactory<St> + 'static,
{
    BoxServiceFactory(Box::new(factory))
}

/// Creates a boxed service.
pub fn service<S, St>(service: S) -> BoxService<St, S::Req, S::Res, S::Error>
where
    S: crate::Service<St> + 'static,
    St: 'static,
{
    BoxService(Box::new(service))
}

impl<St, Req, Res, Err> fmt::Debug for BoxService<St, Req, Res, Err> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BoxService").finish()
    }
}

impl<St, Req, Res, Err, Cfg, InitError> fmt::Debug
    for BoxServiceFactory<St, Req, Res, Err, Cfg, InitError>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BoxServiceFactory").finish()
    }
}

trait ServiceObj<St> {
    type Req;
    type Res;
    type Error;

    fn ready<'a>(
        &'a self,
        idx: u32,
        waiters: &'a WaitersRef,
        st: Option<&'a St>,
    ) -> BoxFuture<'a, (), Self::Error>;

    fn call<'a>(
        &'a self,
        req: Self::Req,
        idx: u32,
        waiters: &'a WaitersRef,
        st: &'a St,
    ) -> BoxFuture<'a, Self::Res, Self::Error>;

    fn shutdown<'a>(&'a self) -> Pin<Box<dyn Future<Output = ()> + 'a>>;

    fn poll(&self, cx: &mut Context<'_>) -> Result<(), Self::Error>;
}

impl<S, St> ServiceObj<St> for S
where
    S: crate::Service<St>,
    St: 'static,
{
    type Req = S::Req;
    type Res = S::Res;
    type Error = S::Error;

    #[inline]
    fn ready<'a>(
        &'a self,
        idx: u32,
        waiters: &'a WaitersRef,
        st: Option<&'a St>,
    ) -> BoxFuture<'a, (), Self::Error> {
        Box::pin(async move {
            ReadyCtx::<'a, S, St>::new(idx, waiters, st)
                .ready(self)
                .await
        })
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
        st: &'a St,
    ) -> BoxFuture<'a, Self::Res, Self::Error> {
        Box::pin(async move {
            Ctx::<'a, S, St>::new(idx, waiters, st)
                .call_nowait(self, req)
                .await
        })
    }

    #[inline]
    fn poll(&self, cx: &mut Context<'_>) -> Result<(), Self::Error> {
        crate::Service::poll(self, cx)
    }
}

trait ServiceFactoryObj<St, Cfg> {
    type Req;
    type Res;
    type Error;
    type InitError;

    fn create<'a>(
        &'a self,
        cfg: &'a Cfg,
    ) -> BoxFuture<'a, BoxService<St, Self::Req, Self::Res, Self::Error>, Self::InitError>
    where
        Cfg: 'a,
        St: 'a;
}

impl<F, St, Cfg> ServiceFactoryObj<St, Cfg> for F
where
    St: 'static,
    Cfg: 'static,
    F: crate::ServiceFactory<St, InitCfg = Cfg>,
    F::Service: 'static,
{
    type Req = F::Req;
    type Res = F::Res;
    type Error = F::Error;
    type InitError = F::InitError;

    #[inline]
    fn create<'a>(
        &'a self,
        cfg: &'a Cfg,
    ) -> BoxFuture<'a, BoxService<St, Self::Req, Self::Res, Self::Error>, Self::InitError>
    where
        Cfg: 'a,
        St: 'a,
    {
        let fut = crate::ServiceFactory::create(self, cfg);
        Box::pin(async move { fut.await.map(service) })
    }
}

impl<St, Req, Res, Err> crate::Service<St> for BoxService<St, Req, Res, Err>
where
    Req: 'static,
{
    type Req = Req;
    type Res = Res;
    type Error = Err;

    #[inline]
    async fn ready(&self, ctx: ReadyCtx<'_, Self, St>) -> Result<(), Self::Error> {
        let (idx, waiters, st) = ctx.inner();
        self.0.ready(idx, waiters, st).await
    }

    #[inline]
    async fn shutdown(&self) {
        self.0.shutdown().await;
    }

    #[inline]
    async fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> Result<Res, Err> {
        let (idx, waiters, st) = ctx.inner();
        self.0.call(req, idx, waiters, st).await
    }

    #[inline]
    fn poll(&self, cx: &mut Context<'_>) -> Result<(), Self::Error> {
        self.0.poll(cx)
    }
}

impl<St, Req, Res, Err, InitCfg, InitError> crate::ServiceFactory<St>
    for BoxServiceFactory<St, Req, Res, Err, InitCfg, InitError>
where
    Req: 'static,
{
    type Req = Req;
    type Res = Res;
    type Error = Err;

    type Service = BoxService<St, Req, Res, Err>;
    type InitCfg = InitCfg;
    type InitError = InitError;

    #[inline]
    async fn create(&self, cfg: &InitCfg) -> Result<Self::Service, Self::InitError> {
        self.0.create(cfg).await
    }
}
