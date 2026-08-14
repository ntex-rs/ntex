use std::{fmt, future::Future, pin::Pin, task::Context};

use crate::ctx::{ServiceCtx, WaitersRef};

type BoxFuture<'a, I, E> = Pin<Box<dyn Future<Output = Result<I, E>> + 'a>>;

pub struct BoxService<Req, Res, Err, Data = ()>(
    Box<dyn ServiceObj<Req, Response = Res, Error = Err, Data = Data>>,
);

pub struct BoxServiceFactory<Cfg, Req, Res, Err, InitErr, Data = (), ServiceData = ()>(
    Box<
        dyn ServiceFactoryObj<
                Req,
                Cfg,
                Response = Res,
                Error = Err,
                InitError = InitErr,
                Data = Data,
                ServiceData = ServiceData,
            >,
    >,
);

/// Creates a boxed service factory.
pub fn factory<F, R, C>(
    factory: F,
) -> BoxServiceFactory<
    C,
    R,
    F::Response,
    F::Error,
    F::InitError,
    F::Data,
    <F::Service as crate::Service<R>>::Data,
>
where
    R: 'static,
    C: 'static,
    F: crate::ServiceFactory<R, C> + 'static,
    F::Service: 'static,
    F::Data: 'static,
    <F::Service as crate::Service<R>>::Data: 'static,
{
    BoxServiceFactory(Box::new(factory))
}

/// Creates a boxed service.
pub fn service<S, R>(service: S) -> BoxService<R, S::Response, S::Error, S::Data>
where
    R: 'static,
    S: crate::Service<R> + 'static,
    S::Data: 'static,
{
    BoxService(Box::new(service))
}

impl<Req, Res, Err, Data> fmt::Debug for BoxService<Req, Res, Err, Data> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BoxService").finish()
    }
}

impl<Cfg, Req, Res, Err, InitErr, Data, ServiceData> fmt::Debug
    for BoxServiceFactory<Cfg, Req, Res, Err, InitErr, Data, ServiceData>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BoxServiceFactory").finish()
    }
}

trait ServiceObj<Req> {
    type Response;
    type Error;
    type Data;

    fn ready<'a>(
        &'a self,
        data: &'a Self::Data,
        idx: u32,
        waiters: &'a WaitersRef,
    ) -> BoxFuture<'a, (), Self::Error>;

    fn call<'a>(
        &'a self,
        req: Req,
        data: &'a Self::Data,
        idx: u32,
        waiters: &'a WaitersRef,
    ) -> BoxFuture<'a, Self::Response, Self::Error>;

    fn shutdown<'a>(
        &'a self,
        data: &'a Self::Data,
    ) -> Pin<Box<dyn Future<Output = ()> + 'a>>;

    fn poll(&self, data: &Self::Data, cx: &mut Context<'_>) -> Result<(), Self::Error>;
}

impl<S, Req> ServiceObj<Req> for S
where
    S: crate::Service<Req>,
    Req: 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Data = S::Data;

    #[inline]
    fn ready<'a>(
        &'a self,
        data: &'a Self::Data,
        idx: u32,
        waiters: &'a WaitersRef,
    ) -> BoxFuture<'a, (), Self::Error> {
        Box::pin(async move {
            ServiceCtx::<'a, S>::new(idx, waiters)
                .ready(self, data)
                .await
        })
    }

    #[inline]
    fn shutdown<'a>(
        &'a self,
        data: &'a Self::Data,
    ) -> Pin<Box<dyn Future<Output = ()> + 'a>> {
        Box::pin(crate::Service::shutdown(self, data))
    }

    #[inline]
    fn call<'a>(
        &'a self,
        req: Req,
        data: &'a Self::Data,
        idx: u32,
        waiters: &'a WaitersRef,
    ) -> BoxFuture<'a, Self::Response, Self::Error> {
        Box::pin(async move {
            ServiceCtx::<'a, S>::new(idx, waiters)
                .call_nowait(self, req, data)
                .await
        })
    }

    #[inline]
    fn poll(&self, data: &Self::Data, cx: &mut Context<'_>) -> Result<(), Self::Error> {
        crate::Service::poll(self, data, cx)
    }
}

trait ServiceFactoryObj<Req, Cfg> {
    type Response;
    type Error;
    type InitError;
    type Data;
    type ServiceData;

    fn create<'a>(
        &'a self,
        cfg: Cfg,
    ) -> BoxFuture<
        'a,
        BoxService<Req, Self::Response, Self::Error, Self::ServiceData>,
        Self::InitError,
    >
    where
        Cfg: 'a;

    fn map_data<'a>(
        &'a self,
        cfg: &'a Cfg,
        data: &'a Self::Data,
    ) -> BoxFuture<'a, Self::ServiceData, Self::InitError>;
}

impl<F, Req, Cfg> ServiceFactoryObj<Req, Cfg> for F
where
    Cfg: 'static,
    Req: 'static,
    F: crate::ServiceFactory<Req, Cfg>,
    F::Service: 'static,
    F::Data: 'static,
    <F::Service as crate::Service<Req>>::Data: 'static,
{
    type Response = F::Response;
    type Error = F::Error;
    type InitError = F::InitError;
    type Data = F::Data;
    type ServiceData = <F::Service as crate::Service<Req>>::Data;

    #[inline]
    fn create<'a>(
        &'a self,
        cfg: Cfg,
    ) -> BoxFuture<
        'a,
        BoxService<Req, Self::Response, Self::Error, Self::ServiceData>,
        Self::InitError,
    >
    where
        Cfg: 'a,
    {
        Box::pin(async move { crate::ServiceFactory::create(self, cfg).await.map(service) })
    }

    #[inline]
    fn map_data<'a>(
        &'a self,
        cfg: &'a Cfg,
        data: &'a Self::Data,
    ) -> BoxFuture<'a, Self::ServiceData, Self::InitError> {
        Box::pin(crate::ServiceFactory::map_data(self, cfg, data))
    }
}

impl<Req, Res, Err, Data> crate::Service<Req> for BoxService<Req, Res, Err, Data>
where
    Req: 'static,
{
    type Response = Res;
    type Error = Err;
    type Data = Data;

    #[inline]
    async fn ready(
        &self,
        data: &Self::Data,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<(), Self::Error> {
        let (idx, waiters) = ctx.inner();
        self.0.ready(data, idx, waiters).await
    }

    #[inline]
    async fn shutdown(&self, data: &Self::Data) {
        self.0.shutdown(data).await;
    }

    #[inline]
    async fn call(
        &self,
        req: Req,
        data: &Self::Data,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<Res, Err> {
        let (idx, waiters) = ctx.inner();
        self.0.call(req, data, idx, waiters).await
    }

    #[inline]
    fn poll(&self, data: &Self::Data, cx: &mut Context<'_>) -> Result<(), Self::Error> {
        self.0.poll(data, cx)
    }
}

impl<C, Req, Res, Err, InitErr, Data, ServiceData> crate::ServiceFactory<Req, C>
    for BoxServiceFactory<C, Req, Res, Err, InitErr, Data, ServiceData>
where
    Req: 'static,
{
    type Response = Res;
    type Error = Err;
    type Service = BoxService<Req, Res, Err, ServiceData>;
    type InitError = InitErr;
    type Data = Data;

    #[inline]
    async fn create(&self, cfg: C) -> Result<Self::Service, Self::InitError> {
        self.0.create(cfg).await
    }

    #[inline]
    async fn map_data(
        &self,
        cfg: &C,
        data: &Self::Data,
    ) -> Result<ServiceData, Self::InitError> {
        self.0.map_data(cfg, data).await
    }
}
