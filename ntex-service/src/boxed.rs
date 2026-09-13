use std::{fmt, rc::Rc};

use crate::ctx::{Ctx, WaitersRef};
use crate::{Service, ServiceFactory, util::BoxFuture};

// ============================ Service =============================

/// Boxed service.
pub struct BoxService<St, Req, Res, Err> {
    inner: Rc<dyn ServiceObj<St, Req, Res = Res, Error = Err>>,
}

/// Creates a boxed service.
pub fn service<S, St, Req, Res, Err>(service: S) -> BoxService<St, Req, Res, Err>
where
    S: Service<St, Req, Res = Res, Error = Err> + 'static,
{
    BoxService::new(service)
}

impl<St, Req, Res, Err> BoxService<St, Req, Res, Err> {
    /// Creates a boxed service.
    pub fn new<S>(service: S) -> Self
    where
        S: Service<St, Req, Res = Res, Error = Err> + 'static,
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

impl<St, Req, Res, Err> Service<St, Req> for BoxService<St, Req, Res, Err> {
    type Res = Res;
    type Error = Err;

    #[inline]
    async fn ready(&self, ctx: Ctx<'_, Self, St>) -> Result<(), Self::Error> {
        let (idx, waiters, st) = ctx.inner();
        self.inner.ready(idx, waiters, st).await
    }

    #[inline]
    async fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> Result<Res, Err> {
        let (idx, waiters, st) = ctx.inner();
        self.inner.call(req, idx, waiters, st).await
    }

    #[inline]
    async fn shutdown(&self, ctx: Ctx<'_, Self, St>) {
        let (idx, waiters, st) = ctx.inner();
        self.inner.shutdown(idx, waiters, st).await;
    }
}

trait ServiceObj<St, Req> {
    type Res;
    type Error;

    fn ready<'a>(
        &'a self,
        i: u32,
        w: &'a WaitersRef,
        s: &'a St,
    ) -> BoxFuture<'a, Result<(), Self::Error>>;

    fn call<'a>(
        &'a self,
        r: Req,
        i: u32,
        w: &'a WaitersRef,
        s: &'a St,
    ) -> BoxFuture<'a, Result<Self::Res, Self::Error>>
    where
        Req: 'a;

    fn shutdown<'a>(&'a self, idx: u32, waiters: &'a WaitersRef, st: &'a St) -> BoxFuture<'a, ()>
    where
        St: 'a,
        Req: 'a;
}

impl<S, St, Req> ServiceObj<St, Req> for S
where
    S: Service<St, Req>,
{
    type Res = S::Res;
    type Error = S::Error;

    #[inline]
    fn ready<'a>(
        &'a self,
        idx: u32,
        waiters: &'a WaitersRef,
        st: &'a St,
    ) -> BoxFuture<'a, Result<(), Self::Error>> {
        Box::pin(async move { Ctx::<'a, S, St>::new(idx, waiters, st).ready(self).await })
    }

    #[inline]
    fn call<'a>(
        &'a self,
        req: Req,
        idx: u32,
        waiters: &'a WaitersRef,
        st: &'a St,
    ) -> BoxFuture<'a, Result<S::Res, S::Error>>
    where
        Req: 'a,
    {
        Box::pin(async move {
            Ctx::<'a, S, St>::new(idx, waiters, st)
                .call_nowait(self, req)
                .await
        })
    }

    #[inline]
    fn shutdown<'a>(&'a self, idx: u32, waiters: &'a WaitersRef, st: &'a St) -> BoxFuture<'a, ()>
    where
        St: 'a,
        Req: 'a,
    {
        Box::pin(async move { Ctx::<'a, S, St>::new(idx, waiters, st).shutdown(self).await })
    }
}

// ============================ ServiceFactory =============================

/// Boxed service factory.
pub struct BoxServiceFactory<St, Req, Res, Err, InitErr> {
    inner: Rc<dyn ServiceFactoryObj<St, Req, Res = Res, Error = Err, InitErr = InitErr>>,
}

/// Creates a boxed service factory.
pub fn factory<Sf, St, Req>(
    factory: Sf,
) -> BoxServiceFactory<St, Req, Sf::Res, Sf::Error, Sf::InitError>
where
    Sf: ServiceFactory<St, Req> + 'static,
    St: 'static,
    Req: 'static,
{
    BoxServiceFactory::new(factory)
}

impl<St, Req, Res, Err, InitErr> BoxServiceFactory<St, Req, Res, Err, InitErr>
where
    St: 'static,
    Req: 'static,
{
    /// Creates a boxed service factory.
    pub fn new<Sf>(factory: Sf) -> Self
    where
        Sf: ServiceFactory<St, Req, Res = Res, Error = Err, InitError = InitErr> + 'static,
        St: 'static,
        Req: 'static,
    {
        Self {
            inner: Rc::new(factory),
        }
    }
}

impl<St, Req, Res, Err, InitErr> ServiceFactory<St, Req>
    for BoxServiceFactory<St, Req, Res, Err, InitErr>
where
    Req: 'static,
{
    type Res = Res;
    type Error = Err;

    type Service = BoxService<St, Req, Res, Err>;
    type InitError = InitErr;

    #[inline]
    async fn create(&self, st: &St) -> Result<Self::Service, Self::InitError> {
        self.inner.create(st).await
    }
}

impl<St, Req, Res, Err, InitErr> Clone for BoxServiceFactory<St, Req, Res, Err, InitErr> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<St, Req, Res, Err, InitErr> fmt::Debug for BoxServiceFactory<St, Req, Res, Err, InitErr> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BoxServiceFactory").finish()
    }
}

trait ServiceFactoryObj<St, Req> {
    type Res;
    type Error;
    type InitErr;

    fn create<'a>(
        &'a self,
        st: &'a St,
    ) -> BoxFuture<'a, Result<BoxService<St, Req, Self::Res, Self::Error>, Self::InitErr>>
    where
        Req: 'a;
}

impl<Sf, St, Req> ServiceFactoryObj<St, Req> for Sf
where
    St: 'static,
    Req: 'static,
    Sf: ServiceFactory<St, Req> + 'static,
{
    type Res = Sf::Res;
    type Error = Sf::Error;
    type InitErr = Sf::InitError;

    #[inline]
    fn create<'a>(
        &'a self,
        st: &'a St,
    ) -> BoxFuture<'a, Result<BoxService<St, Req, Self::Res, Self::Error>, Self::InitErr>>
    where
        Req: 'a,
    {
        let fut = ServiceFactory::create(self, st);
        Box::pin(async move { fut.await.map(service) })
    }
}
