#![allow(clippy::type_complexity)]
use std::{fmt, future::Future, pin::Pin, task::Context};

use crate::ctx::{Ctx, ReadyCtx, WaitersRef};
use crate::{Service, ServiceFactory};

type BoxFuture<'a, I, E> = Pin<Box<dyn Future<Output = Result<I, E>> + 'a>>;
pub struct BoxService<St, Req, Res, Err>(
    Box<dyn ServiceObj<St = St, Req = Req, Res = Res, Error = Err>>,
);
pub struct BoxServiceFactory<St, Req, Res, Err, InitCfg, InitError>(
    Box<
        dyn ServiceFactoryObj<
                Req,
                St = St,
                Res = Res,
                Error = Err,
                InitCfg = InitCfg,
                InitError = InitError,
            >,
    >,
);

/// Creates a boxed service factory.
pub fn factory<Sf, Req>(
    factory: Sf,
) -> BoxServiceFactory<Sf::St, Req, Sf::Res, Sf::Error, Sf::InitCfg, Sf::InitError>
where
    Sf: ServiceFactory<Req> + 'static,
    Req: 'static,
{
    BoxServiceFactory(Box::new(factory))
}

/// Creates a boxed service.
pub fn service<S>(service: S) -> BoxService<S::St, S::Req, S::Res, S::Error>
where
    S: Service + 'static,
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

trait ServiceObj {
    type St;
    type Req;
    type Res;
    type Error;

    fn ready<'a>(
        &'a self,
        idx: u32,
        waiters: &'a WaitersRef,
        st: Option<&'a Self::St>,
    ) -> BoxFuture<'a, (), Self::Error>;

    fn call<'a>(
        &'a self,
        req: Self::Req,
        idx: u32,
        waiters: &'a WaitersRef,
        st: &'a Self::St,
    ) -> BoxFuture<'a, Self::Res, Self::Error>;

    fn shutdown<'a>(&'a self) -> Pin<Box<dyn Future<Output = ()> + 'a>>;

    fn poll(&self, cx: &mut Context<'_>) -> Result<(), Self::Error>;
}

impl<S> ServiceObj for S
where
    S: Service,
{
    type St = S::St;
    type Req = S::Req;
    type Res = S::Res;
    type Error = S::Error;

    #[inline]
    fn ready<'a>(
        &'a self,
        idx: u32,
        waiters: &'a WaitersRef,
        st: Option<&'a Self::St>,
    ) -> BoxFuture<'a, (), Self::Error> {
        Box::pin(async move { ReadyCtx::<'a, S>::new(idx, waiters, st).ready(self).await })
    }

    #[inline]
    fn shutdown<'a>(&'a self) -> Pin<Box<dyn Future<Output = ()> + 'a>> {
        Box::pin(Service::shutdown(self))
    }

    #[inline]
    fn call<'a>(
        &'a self,
        req: S::Req,
        idx: u32,
        waiters: &'a WaitersRef,
        st: &'a Self::St,
    ) -> BoxFuture<'a, S::Res, S::Error> {
        Box::pin(async move {
            Ctx::<'a, S>::new(idx, waiters, st)
                .call_nowait(self, req)
                .await
        })
    }

    #[inline]
    fn poll(&self, cx: &mut Context<'_>) -> Result<(), Self::Error> {
        Service::poll(self, cx)
    }
}

trait ServiceFactoryObj<Req> {
    type St;
    type Res;
    type Error;
    type InitCfg;
    type InitError;

    fn create<'a>(
        &'a self,
        cfg: &'a Self::InitCfg,
    ) -> BoxFuture<'a, BoxService<Self::St, Req, Self::Res, Self::Error>, Self::InitError>
    where
        Req: 'a;
}

impl<Sf, Req> ServiceFactoryObj<Req> for Sf
where
    Req: 'static,
    Sf: ServiceFactory<Req> + 'static,
{
    type St = Sf::St;
    type Res = Sf::Res;
    type Error = Sf::Error;
    type InitCfg = Sf::InitCfg;
    type InitError = Sf::InitError;

    #[inline]
    fn create<'a>(
        &'a self,
        cfg: &'a Self::InitCfg,
    ) -> BoxFuture<'a, BoxService<Self::St, Req, Self::Res, Self::Error>, Self::InitError>
    where
        Req: 'a,
    {
        let fut = ServiceFactory::create(self, cfg);
        Box::pin(async move { fut.await.map(service) })
    }
}

impl<St, Req, Res, Err> Service for BoxService<St, Req, Res, Err> {
    type St = St;
    type Req = Req;
    type Res = Res;
    type Error = Err;

    #[inline]
    async fn ready(&self, ctx: ReadyCtx<'_, Self>) -> Result<(), Self::Error> {
        let (idx, waiters, st) = ctx.inner();
        self.0.ready(idx, waiters, st).await
    }

    #[inline]
    async fn shutdown(&self) {
        self.0.shutdown().await;
    }

    #[inline]
    async fn call(&self, req: Req, ctx: Ctx<'_, Self>) -> Result<Res, Err> {
        let (idx, waiters, st) = ctx.inner();
        self.0.call(req, idx, waiters, st).await
    }

    #[inline]
    fn poll(&self, cx: &mut Context<'_>) -> Result<(), Self::Error> {
        self.0.poll(cx)
    }
}

impl<St, Req, Res, Err, InitCfg, InitError> ServiceFactory<Req>
    for BoxServiceFactory<St, Req, Res, Err, InitCfg, InitError>
where
    Req: 'static,
{
    type St = St;
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
