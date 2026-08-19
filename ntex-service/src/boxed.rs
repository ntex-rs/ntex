use std::{fmt, future::Future, pin::Pin, rc::Rc};

use crate::ctx::{Ctx, ReadyCtx, WaitersRef};
use crate::{Service, ServiceFactory};

type BoxFuture<'a, I, E> = Pin<Box<dyn Future<Output = Result<I, E>> + 'a>>;

// ============================ Service =============================

/// Boxed service.
pub struct BoxService<St, Req, Res, Err> {
    inner: Rc<dyn ServiceObj<St, Req = Req, Res = Res, Error = Err>>,
}

/// Creates a boxed service.
pub fn service<S, St>(service: S) -> BoxService<St, S::Req, S::Res, S::Error>
where
    S: Service<St> + 'static,
{
    BoxService::new(service)
}

impl<St, Req, Res, Err> BoxService<St, Req, Res, Err> {
    /// Creates a boxed service.
    pub fn new<S>(service: S) -> Self
    where
        S: Service<St, Req = Req, Res = Res, Error = Err> + 'static,
    {
        BoxService {
            inner: Rc::new(service),
        }
    }
}

impl<St, Req, Res, Err> Clone for BoxService<St, Req, Res, Err> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<St, Req, Res, Err> fmt::Debug for BoxService<St, Req, Res, Err> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BoxService").finish()
    }
}

impl<St, Req, Res, Err> Service<St> for BoxService<St, Req, Res, Err> {
    type Req = Req;
    type Res = Res;
    type Error = Err;

    #[inline]
    async fn ready(&self, ctx: ReadyCtx<'_, Self, St>) -> Result<(), Self::Error> {
        let (idx, waiters, st) = ctx.inner();
        self.inner.ready(idx, waiters, st).await
    }

    #[inline]
    async fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> Result<Res, Err> {
        let (idx, waiters, st) = ctx.inner();
        self.inner.call(req, idx, waiters, st).await
    }

    #[inline]
    async fn shutdown(&self) {
        self.inner.shutdown().await;
    }
}

trait ServiceObj<St> {
    type Req;
    type Res;
    type Error;

    fn ready<'a>(&'a self, i: u32, w: &'a WaitersRef, s: &'a St) -> BoxFuture<'a, (), Self::Error>;

    fn shutdown<'a>(&'a self) -> Pin<Box<dyn Future<Output = ()> + 'a>>
    where
        St: 'a;

    fn call<'a>(
        &'a self,
        r: Self::Req,
        i: u32,
        w: &'a WaitersRef,
        s: &'a St,
    ) -> BoxFuture<'a, Self::Res, Self::Error>;
}

impl<S, St> ServiceObj<St> for S
where
    S: Service<St>,
{
    type Req = S::Req;
    type Res = S::Res;
    type Error = S::Error;

    #[inline]
    fn ready<'a>(&'a self, i: u32, w: &'a WaitersRef, s: &'a St) -> BoxFuture<'a, (), Self::Error> {
        Box::pin(async move { ReadyCtx::<'a, S, St>::new(i, w, s).ready(self).await })
    }

    #[inline]
    fn call<'a>(
        &'a self,
        req: S::Req,
        idx: u32,
        waiters: &'a WaitersRef,
        st: &'a St,
    ) -> BoxFuture<'a, S::Res, S::Error> {
        Box::pin(async move {
            Ctx::<'a, S, St>::new(idx, waiters, st)
                .call_nowait(self, req)
                .await
        })
    }

    #[inline]
    fn shutdown<'a>(&'a self) -> Pin<Box<dyn Future<Output = ()> + 'a>>
    where
        St: 'a,
    {
        Box::pin(Service::shutdown(self))
    }
}

// ============================ ServiceFactory =============================

/// Boxed service factory.
pub struct BoxServiceFactory<St, Req, Res, Err, Cfg, InitError> {
    inner: Rc<dyn ServiceFactoryObj<St, Req, Cfg, Res = Res, Error = Err, InitErr = InitError>>,
}

/// Creates a boxed service factory.
pub fn factory<Sf, St, Req, Cfg>(
    factory: Sf,
) -> BoxServiceFactory<St, Req, Sf::Res, Sf::Error, Cfg, Sf::InitError>
where
    Sf: ServiceFactory<St, Req, Cfg> + 'static,
    St: 'static,
    Req: 'static,
    Cfg: 'static,
{
    BoxServiceFactory::new(factory)
}

impl<St, Req, Res, Err, Cfg, InitErr> BoxServiceFactory<St, Req, Res, Err, Cfg, InitErr>
where
    St: 'static,
    Req: 'static,
    Cfg: 'static,
{
    /// Creates a boxed service factory.
    pub fn new<Sf>(factory: Sf) -> Self
    where
        Sf: ServiceFactory<St, Req, Cfg, Res = Res, Error = Err, InitError = InitErr> + 'static,
        St: 'static,
        Req: 'static,
    {
        Self {
            inner: Rc::new(factory),
        }
    }
}

impl<St, Req, Res, Err, Cfg, InitError> ServiceFactory<St, Req, Cfg>
    for BoxServiceFactory<St, Req, Res, Err, Cfg, InitError>
where
    Req: 'static,
{
    type Res = Res;
    type Error = Err;

    type Service = BoxService<St, Req, Res, Err>;
    type InitError = InitError;

    #[inline]
    async fn create(&self, cfg: &Cfg) -> Result<Self::Service, Self::InitError> {
        self.inner.create(cfg).await
    }
}

impl<St, Req, Res, Err, Cfg, InitError> fmt::Debug
    for BoxServiceFactory<St, Req, Res, Err, Cfg, InitError>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BoxServiceFactory").finish()
    }
}

trait ServiceFactoryObj<St, Req, Cfg> {
    type Res;
    type Error;
    type InitErr;

    fn create<'a>(
        &'a self,
        cfg: &'a Cfg,
    ) -> BoxFuture<'a, BoxService<St, Req, Self::Res, Self::Error>, Self::InitErr>
    where
        Req: 'a;
}

impl<Sf, St, Req, Cfg> ServiceFactoryObj<St, Req, Cfg> for Sf
where
    St: 'static,
    Req: 'static,
    Cfg: 'static,
    Sf: ServiceFactory<St, Req, Cfg> + 'static,
{
    type Res = Sf::Res;
    type Error = Sf::Error;
    type InitErr = Sf::InitError;

    #[inline]
    fn create<'a>(
        &'a self,
        cfg: &'a Cfg,
    ) -> BoxFuture<'a, BoxService<St, Req, Self::Res, Self::Error>, Self::InitErr>
    where
        Req: 'a,
    {
        let fut = ServiceFactory::create(self, cfg);
        Box::pin(async move { fut.await.map(service) })
    }
}
