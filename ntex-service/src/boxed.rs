use std::{future::Future, pin::Pin, rc::Rc, task::Context, task::Poll};

pub type BoxFuture<'a, I, E> = Pin<Box<dyn Future<Output = Result<I, E>> + 'a>>;

/// Create boxed service factory
pub fn factory<F, C>(
    factory: F,
) -> BoxServiceFactory<C, F::Request, F::Response, F::Error, F::InitError>
where
    F: crate::ServiceFactory<C> + 'static,
    F::Request: 'static,
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

/// Create rc service
pub fn rcservice<S, R>(service: S) -> RcService<R, S::Response, S::Error>
where
    R: 'static,
    S: crate::Service<R> + 'static,
{
    RcService(Rc::new(service))
}

trait ServiceObj<Req> {
    type Response;
    type Error;

    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>>;

    fn poll_shutdown(&self, cx: &mut Context<'_>, is_error: bool) -> Poll<()>;

    fn call<'a>(&'a self, req: Req) -> BoxFuture<'a, Self::Response, Self::Error>
    where
        Req: 'a;
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
    fn poll_shutdown(&self, cx: &mut Context<'_>, is_error: bool) -> Poll<()> {
        crate::Service::poll_shutdown(self, cx, is_error)
    }

    #[inline]
    fn call<'a>(&'a self, req: Req) -> BoxFuture<'a, Self::Response, Self::Error>
    where
        Req: 'a,
    {
        Box::pin(crate::Service::call(self, req))
    }
}

trait ServiceFactoryObj<Cfg, Req, Res, Err, InitErr> {
    fn create<'a>(
        &'a self,
        cfg: &'a Cfg,
    ) -> BoxFuture<'a, BoxService<Req, Res, Err>, InitErr>;
}

impl<F, Cfg, Req, Res, Err, InitErr> ServiceFactoryObj<Cfg, Req, Res, Err, InitErr> for F
where
    Req: 'static,
    F: crate::ServiceFactory<
        Cfg,
        Request = Req,
        Response = Res,
        Error = Err,
        InitError = InitErr,
    >,
    F::Request: 'static,
    F::Service: 'static,
{
    #[inline]
    fn create<'a>(
        &'a self,
        cfg: &'a Cfg,
    ) -> BoxFuture<'a, BoxService<Req, Res, Err>, InitErr> {
        Box::pin(async move { crate::ServiceFactory::create(self, cfg).await.map(service) })
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
    fn poll_shutdown(&self, cx: &mut Context<'_>, is_error: bool) -> Poll<()> {
        self.0.poll_shutdown(cx, is_error)
    }

    #[inline]
    fn call<'a>(&'a self, req: Req) -> Self::Future<'a> {
        self.0.call(req)
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
    fn poll_shutdown(&self, cx: &mut Context<'_>, is_error: bool) -> Poll<()> {
        self.0.poll_shutdown(cx, is_error)
    }

    #[inline]
    fn call<'a>(&'a self, req: Req) -> Self::Future<'a> {
        self.0.call(req)
    }
}

pub struct BoxServiceFactory<Cfg, Req, Res, Err, InitErr>(
    Box<dyn ServiceFactoryObj<Cfg, Req, Res, Err, InitErr>>,
);

impl<C, Req, Res, Err, InitErr> crate::ServiceFactory<C>
    for BoxServiceFactory<C, Req, Res, Err, InitErr>
where
    Req: 'static,
{
    type Request = Req;
    type Response = Res;
    type Error = Err;

    type Service = BoxService<Req, Res, Err>;
    type InitError = InitErr;
    type Future<'f> = BoxFuture<'f, Self::Service, InitErr> where Self: 'f, Self::Request: 'f, C: 'f;

    #[inline]
    fn create<'a>(&'a self, cfg: &'a C) -> Self::Future<'a> {
        self.0.create(cfg)
    }
}
