use std::task::{Context, Poll, Waker};
use std::{cell::RefCell, future::Future, pin::Pin, rc::Rc};

use crate::Ctx;

pub type BoxFuture<'a, I, E> = Pin<Box<dyn Future<Output = Result<I, E>> + 'a>>;

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

/// Create rc boxed service factory
pub fn rcfactory<F, R, C>(
    factory: F,
) -> RcServiceFactory<C, R, F::Response, F::Error, F::InitError>
where
    R: 'static,
    C: 'static,
    F: crate::ServiceFactory<R, C> + 'static,
    F::Service: 'static,
{
    RcServiceFactory(Rc::new(factory))
}

/// Create boxed service
pub fn service<S, R>(service: S) -> BoxService<R, S::Response, S::Error>
where
    R: 'static,
    S: crate::Service<R> + 'static,
{
    BoxService(Box::new(service))
}

/// Create rc service
pub fn rcservice<S, R>(service: S) -> RcService<R, S::Response, S::Error>
where
    R: 'static,
    S: crate::Service<R> + 'static,
{
    RcService(Rc::new(service))
}

pub trait ServiceObj<Req> {
    type Response;
    type Error;

    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>>;

    fn poll_shutdown(&self, cx: &mut Context<'_>) -> Poll<()>;

    fn call<'a>(
        &'a self,
        req: Req,
        idx: usize,
        waiters: &'a Rc<RefCell<slab::Slab<Option<Waker>>>>,
    ) -> BoxFuture<'a, Self::Response, Self::Error>;
}

impl<S, Req> ServiceObj<Req> for S
where
    Req: 'static,
    S: crate::Service<Req>,
{
    type Response = S::Response;
    type Error = S::Error;

    #[inline]
    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        crate::Service::poll_ready(self, cx)
    }

    #[inline]
    fn poll_shutdown(&self, cx: &mut Context<'_>) -> Poll<()> {
        crate::Service::poll_shutdown(self, cx)
    }

    #[inline]
    fn call<'a>(
        &'a self,
        req: Req,
        idx: usize,
        waiters: &'a Rc<RefCell<slab::Slab<Option<Waker>>>>,
    ) -> BoxFuture<'a, Self::Response, Self::Error> {
        Box::pin(Ctx::<'a, S>::new(idx, waiters).call_nowait(self, req))
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

trait ServiceFactoryRcObj<Req, Cfg> {
    type Response;
    type Error;
    type InitError;

    fn create<'a>(
        &'a self,
        cfg: Cfg,
    ) -> BoxFuture<'a, RcService<Req, Self::Response, Self::Error>, Self::InitError>
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

impl<F, Req, Cfg> ServiceFactoryRcObj<Req, Cfg> for F
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
    ) -> BoxFuture<'a, RcService<Req, Self::Response, Self::Error>, Self::InitError>
    where
        Cfg: 'a,
    {
        let fut = crate::ServiceFactory::create(self, cfg);
        Box::pin(async move { fut.await.map(rcservice) })
    }
}

pub struct BoxService<Req, Res, Err>(Box<dyn ServiceObj<Req, Response = Res, Error = Err>>);

impl<Req, Res, Err> crate::Service<Req> for BoxService<Req, Res, Err>
where
    Req: 'static,
{
    type Response = Res;
    type Error = Err;
    type Future<'f> = BoxFuture<'f, Res, Err> where Self: 'f, Req: 'f;

    #[inline]
    fn poll_ready(&self, ctx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.0.poll_ready(ctx)
    }

    #[inline]
    fn poll_shutdown(&self, cx: &mut Context<'_>) -> Poll<()> {
        self.0.poll_shutdown(cx)
    }

    #[inline]
    fn call<'a>(&'a self, req: Req, ctx: Ctx<'a, Self>) -> Self::Future<'a> {
        let (index, waiters) = ctx.into_inner();
        self.0.call(req, index, waiters)
    }
}

pub struct RcService<Req, Res, Err>(Rc<dyn ServiceObj<Req, Response = Res, Error = Err>>);

impl<Req, Res, Err> Clone for RcService<Req, Res, Err> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<Req, Res, Err> crate::Service<Req> for RcService<Req, Res, Err>
where
    Req: 'static,
{
    type Response = Res;
    type Error = Err;
    type Future<'f> = BoxFuture<'f, Res, Err> where Self: 'f, Req: 'f;

    #[inline]
    fn poll_ready(&self, ctx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.0.poll_ready(ctx)
    }

    #[inline]
    fn poll_shutdown(&self, cx: &mut Context<'_>) -> Poll<()> {
        self.0.poll_shutdown(cx)
    }

    #[inline]
    fn call<'a>(&'a self, req: Req, ctx: Ctx<'a, Self>) -> Self::Future<'a> {
        let (index, waiters) = ctx.into_inner();
        self.0.call(req, index, waiters)
    }
}

pub struct BoxServiceFactory<Cfg, Req, Res, Err, InitErr>(
    Box<dyn ServiceFactoryObj<Req, Cfg, Response = Res, Error = Err, InitError = InitErr>>,
);

impl<C, Req, Res, Err, InitErr> crate::ServiceFactory<Req, C>
    for BoxServiceFactory<C, Req, Res, Err, InitErr>
where
    Req: 'static,
{
    type Response = Res;
    type Error = Err;

    type Service = BoxService<Req, Res, Err>;
    type InitError = InitErr;
    type Future<'f> = BoxFuture<'f, Self::Service, InitErr> where Self: 'f, C: 'f;

    #[inline]
    fn create(&self, cfg: C) -> Self::Future<'_> {
        self.0.create(cfg)
    }
}

pub struct RcServiceFactory<Cfg, Req, Res, Err, InitErr>(
    Rc<dyn ServiceFactoryRcObj<Req, Cfg, Response = Res, Error = Err, InitError = InitErr>>,
);

impl<C, Req, Res, Err, InitErr> Clone for RcServiceFactory<C, Req, Res, Err, InitErr> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<C, Req, Res, Err, InitErr> crate::ServiceFactory<Req, C>
    for RcServiceFactory<C, Req, Res, Err, InitErr>
where
    Req: 'static,
{
    type Response = Res;
    type Error = Err;

    type Service = RcService<Req, Res, Err>;
    type InitError = InitErr;
    type Future<'f> = BoxFuture<'f, Self::Service, InitErr> where Self: 'f, C: 'f;

    #[inline]
    fn create(&self, cfg: C) -> Self::Future<'_> {
        self.0.create(cfg)
    }
}
