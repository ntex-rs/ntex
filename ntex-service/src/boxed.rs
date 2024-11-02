use std::{fmt, future::Future, pin::Pin};

use crate::ctx::{ServiceCtx, WaitersRef};

type BoxFuture<'a, I, E> = Pin<Box<dyn Future<Output = Result<I, E>> + 'a>>;
pub struct BoxService<Req, Res, Err>(Box<dyn ServiceObj<Req, Response = Res, Error = Err>>);
pub struct BoxServiceFactory<Cfg, Req, Res, Err, InitErr>(
    Box<dyn ServiceFactoryObj<Req, Cfg, Response = Res, Error = Err, InitError = InitErr>>,
);

/// Create boxed service factory
pub fn factory<F, R, C>(
    factory: F,
) -> BoxServiceFactory<C, R, F::Response, F::Error, F::InitError>
where
    R: 'static,
    C: 'static,
    F: crate::ServiceFactory<R, C> + 'static,
    F::Service: 'static,
{
    BoxServiceFactory(Box::new(factory))
}

/// Create boxed service
pub fn service<S, R>(service: S) -> BoxService<R, S::Response, S::Error>
where
    R: 'static,
    S: crate::Service<R> + 'static,
{
    BoxService(Box::new(service))
}

impl<Req, Res, Err> fmt::Debug for BoxService<Req, Res, Err> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BoxService").finish()
    }
}

impl<Cfg, Req, Res, Err, InitErr> fmt::Debug
    for BoxServiceFactory<Cfg, Req, Res, Err, InitErr>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BoxServiceFactory").finish()
    }
}

trait ServiceObj<Req> {
    type Response;
    type Error;

    fn ready<'a>(
        &'a self,
        idx: u32,
        waiters: &'a WaitersRef,
    ) -> BoxFuture<'a, (), Self::Error>;

    fn call<'a>(
        &'a self,
        req: Req,
        idx: u32,
        waiters: &'a WaitersRef,
    ) -> BoxFuture<'a, Self::Response, Self::Error>;

    fn shutdown<'a>(&'a self) -> Pin<Box<dyn Future<Output = ()> + 'a>>;
}

impl<S, Req> ServiceObj<Req> for S
where
    S: crate::Service<Req>,
    Req: 'static,
{
    type Response = S::Response;
    type Error = S::Error;

    #[inline]
    fn ready<'a>(
        &'a self,
        idx: u32,
        waiters: &'a WaitersRef,
    ) -> BoxFuture<'a, (), Self::Error> {
        Box::pin(async move { ServiceCtx::<'a, S>::new(idx, waiters).ready(self).await })
    }

    #[inline]
    fn shutdown<'a>(&'a self) -> Pin<Box<dyn Future<Output = ()> + 'a>> {
        Box::pin(crate::Service::shutdown(self))
    }

    #[inline]
    fn call<'a>(
        &'a self,
        req: Req,
        idx: u32,
        waiters: &'a WaitersRef,
    ) -> BoxFuture<'a, Self::Response, Self::Error> {
        Box::pin(async move {
            ServiceCtx::<'a, S>::new(idx, waiters)
                .call_nowait(self, req)
                .await
        })
    }
}

trait ServiceFactoryObj<Req, Cfg> {
    type Response;
    type Error;
    type InitError;

    fn create<'a>(
        &'a self,
        cfg: Cfg,
    ) -> BoxFuture<'a, BoxService<Req, Self::Response, Self::Error>, Self::InitError>
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
    type Response = F::Response;
    type Error = F::Error;
    type InitError = F::InitError;

    #[inline]
    fn create<'a>(
        &'a self,
        cfg: Cfg,
    ) -> BoxFuture<'a, BoxService<Req, Self::Response, Self::Error>, Self::InitError>
    where
        Cfg: 'a,
    {
        let fut = crate::ServiceFactory::create(self, cfg);
        Box::pin(async move { fut.await.map(service) })
    }
}

impl<Req, Res, Err> crate::Service<Req> for BoxService<Req, Res, Err>
where
    Req: 'static,
{
    type Response = Res;
    type Error = Err;

    #[inline]
    async fn ready(&self, ctx: ServiceCtx<'_, Self>) -> Result<(), Self::Error> {
        let (idx, waiters) = ctx.inner();
        self.0.ready(idx, waiters).await
    }

    #[inline]
    async fn shutdown(&self) {
        self.0.shutdown().await
    }

    #[inline]
    async fn call(&self, req: Req, ctx: ServiceCtx<'_, Self>) -> Result<Res, Err> {
        let (idx, waiters) = ctx.inner();
        self.0.call(req, idx, waiters).await
    }
}

impl<C, Req, Res, Err, InitErr> crate::ServiceFactory<Req, C>
    for BoxServiceFactory<C, Req, Res, Err, InitErr>
where
    Req: 'static,
{
    type Response = Res;
    type Error = Err;

    type Service = BoxService<Req, Res, Err>;
    type InitError = InitErr;

    #[inline]
    async fn create(&self, cfg: C) -> Result<Self::Service, Self::InitError> {
        self.0.create(cfg).await
    }
}
