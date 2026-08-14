use std::{fmt, future::Future, pin::Pin, task::Context};

use crate::ctx::{ServiceCtx, WaitersRef};
use crate::svc_fct::{ErrorOf, ResponseOf};

type BoxFuture<'a, I, E> = Pin<Box<dyn Future<Output = Result<I, E>> + 'a>>;
pub struct BoxService<Req, Res, Err>(Box<dyn ServiceObj<Req, Response = Res, Error = Err>>);
pub struct BoxServiceFactory<Cfg, Req, Res, Err, InitErr>(
    Box<dyn ServiceFactoryObj<Req, Cfg, Response = Res, Error = Err, InitError = InitErr>>,
);

/// Creates a boxed service factory.
pub fn factory<F, R, C>(
    factory: F,
) -> BoxServiceFactory<C, R, ResponseOf<F, R, C>, ErrorOf<F, R, C>, F::Error>
where
    R: 'static,
    C: 'static,
    F: crate::ServiceFactory<R, C> + crate::Service<C, Data = ()> + 'static,
    F::Response: crate::Service<R, Data = ()> + 'static,
{
    BoxServiceFactory(Box::new(factory))
}

/// Creates a boxed service.
pub fn service<S, R>(service: S) -> BoxService<R, S::Response, S::Error>
where
    R: 'static,
    S: crate::Service<R, Data = ()> + 'static,
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

    fn poll(&self, cx: &mut Context<'_>) -> Result<(), Self::Error>;
}

impl<S, Req> ServiceObj<Req> for S
where
    S: crate::Service<Req, Data = ()>,
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
        Box::pin(async move {
            ServiceCtx::<'a, S>::new(idx, waiters)
                .ready(self, &())
                .await
        })
    }

    #[inline]
    fn shutdown<'a>(&'a self) -> Pin<Box<dyn Future<Output = ()> + 'a>> {
        Box::pin(crate::Service::shutdown(self, &()))
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
                .call_nowait(self, req, &())
                .await
        })
    }

    #[inline]
    fn poll(&self, cx: &mut Context<'_>) -> Result<(), Self::Error> {
        crate::Service::poll(self, &(), cx)
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
    F: crate::ServiceFactory<Req, Cfg> + crate::Service<Cfg, Data = ()>,
    F::Response: crate::Service<Req, Data = ()> + 'static,
{
    type Response = ResponseOf<F, Req, Cfg>;
    type Error = ErrorOf<F, Req, Cfg>;
    type InitError = F::Error;

    #[inline]
    fn create<'a>(
        &'a self,
        cfg: Cfg,
    ) -> BoxFuture<'a, BoxService<Req, Self::Response, Self::Error>, Self::InitError>
    where
        Cfg: 'a,
    {
        Box::pin(async move {
            let (idx, waiters) = WaitersRef::new();
            ServiceCtx::<F>::new(idx, &waiters)
                .call(self, cfg, &())
                .await
                .map(|svc| BoxService(Box::new(svc)))
        })
    }
}

impl<Req, Res, Err> crate::Service<Req> for BoxService<Req, Res, Err>
where
    Req: 'static,
{
    type Response = Res;
    type Error = Err;
    type Data = ();

    #[inline]
    async fn ready(
        &self,
        _: &Self::Data,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<(), Self::Error> {
        let (idx, waiters) = ctx.inner();
        self.0.ready(idx, waiters).await
    }

    #[inline]
    async fn shutdown(&self, _: &Self::Data) {
        self.0.shutdown().await;
    }

    #[inline]
    async fn call(
        &self,
        req: Req,
        _: &Self::Data,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<Res, Err> {
        let (idx, waiters) = ctx.inner();
        self.0.call(req, idx, waiters).await
    }

    #[inline]
    fn poll(&self, _: &Self::Data, cx: &mut Context<'_>) -> Result<(), Self::Error> {
        self.0.poll(cx)
    }
}

impl<C, Req, Res, Err, InitErr> crate::Service<C>
    for BoxServiceFactory<C, Req, Res, Err, InitErr>
where
    Req: 'static,
{
    type Response = BoxService<Req, Res, Err>;
    type Error = InitErr;
    type Data = ();

    #[inline]
    async fn call(
        &self,
        cfg: C,
        _: &Self::Data,
        _: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        self.0.create(cfg).await
    }
}
