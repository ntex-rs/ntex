use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures::future::{ok, Ready};
use pin_project::pin_project;

use crate::{IntoService, IntoServiceFactory, Service, ServiceFactory};

/// Create `ServiceFactory` for function that can act as a `Service`
pub fn service_fn<F, Fut, Req, Res, Err, Cfg>(
    f: F,
) -> impl ServiceFactory<Config = Cfg, Request = Req, Response = Res, Error = Err, InitError = ()>
       + Clone
where
    F: FnMut(Req) -> Fut + Clone,
    Fut: Future<Output = Result<Res, Err>>,
{
    NewServiceFn::new(f)
}

pub fn service_fn2<F, Fut, Req, Res, Err>(
    f: F,
) -> impl Service<Request = Req, Response = Res, Error = Err>
where
    F: FnMut(Req) -> Fut,
    Fut: Future<Output = Result<Res, Err>>,
{
    ServiceFn::new(f)
}

/// Create `ServiceFactory` for function that can produce services
pub fn factory_fn<S, F, Cfg, Fut, Err>(
    f: F,
) -> impl ServiceFactory<
    Config = Cfg,
    Service = S,
    Request = S::Request,
    Response = S::Response,
    Error = S::Error,
    InitError = Err,
    Future = Fut,
>
where
    S: Service,
    F: Fn() -> Fut,
    Fut: Future<Output = Result<S, Err>>,
{
    FnNewServiceNoConfig::new(f)
}

/// Create `ServiceFactory` for function that can produce services with configuration
pub fn factory_fn_cfg<F, Fut, Cfg, Srv, Err>(
    f: F,
) -> impl ServiceFactory<
    Config = Cfg,
    Service = Srv,
    Request = Srv::Request,
    Response = Srv::Response,
    Error = Srv::Error,
    InitError = Err,
>
where
    F: Fn(&Cfg) -> Fut,
    Fut: Future<Output = Result<Srv, Err>>,
    Srv: Service,
{
    FnNewServiceConfig::new(f)
}

pub struct ServiceFn<F, Fut, Req, Res, Err>
where
    F: FnMut(Req) -> Fut,
    Fut: Future<Output = Result<Res, Err>>,
{
    f: F,
    _t: PhantomData<Req>,
}

impl<F, Fut, Req, Res, Err> ServiceFn<F, Fut, Req, Res, Err>
where
    F: FnMut(Req) -> Fut,
    Fut: Future<Output = Result<Res, Err>>,
{
    pub(crate) fn new(f: F) -> Self {
        ServiceFn { f, _t: PhantomData }
    }
}

impl<F, Fut, Req, Res, Err> Clone for ServiceFn<F, Fut, Req, Res, Err>
where
    F: FnMut(Req) -> Fut + Clone,
    Fut: Future<Output = Result<Res, Err>>,
{
    fn clone(&self) -> Self {
        ServiceFn::new(self.f.clone())
    }
}

impl<F, Fut, Req, Res, Err> Service for ServiceFn<F, Fut, Req, Res, Err>
where
    F: FnMut(Req) -> Fut,
    Fut: Future<Output = Result<Res, Err>>,
{
    type Request = Req;
    type Response = Res;
    type Error = Err;
    type Future = Fut;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Req) -> Self::Future {
        (self.f)(req)
    }
}

impl<F, Fut, Req, Res, Err> IntoService<ServiceFn<F, Fut, Req, Res, Err>> for F
where
    F: FnMut(Req) -> Fut,
    Fut: Future<Output = Result<Res, Err>>,
{
    fn into_service(self) -> ServiceFn<F, Fut, Req, Res, Err> {
        ServiceFn::new(self)
    }
}

struct NewServiceFn<F, Fut, Req, Res, Err, Cfg>
where
    F: FnMut(Req) -> Fut,
    Fut: Future<Output = Result<Res, Err>>,
{
    f: F,
    _t: PhantomData<(Req, Cfg)>,
}

impl<F, Fut, Req, Res, Err, Cfg> NewServiceFn<F, Fut, Req, Res, Err, Cfg>
where
    F: FnMut(Req) -> Fut + Clone,
    Fut: Future<Output = Result<Res, Err>>,
{
    fn new(f: F) -> Self {
        NewServiceFn { f, _t: PhantomData }
    }
}

impl<F, Fut, Req, Res, Err, Cfg> Clone for NewServiceFn<F, Fut, Req, Res, Err, Cfg>
where
    F: FnMut(Req) -> Fut + Clone,
    Fut: Future<Output = Result<Res, Err>>,
{
    fn clone(&self) -> Self {
        NewServiceFn::new(self.f.clone())
    }
}

impl<F, Fut, Req, Res, Err, Cfg> ServiceFactory for NewServiceFn<F, Fut, Req, Res, Err, Cfg>
where
    F: FnMut(Req) -> Fut + Clone,
    Fut: Future<Output = Result<Res, Err>>,
{
    type Request = Req;
    type Response = Res;
    type Error = Err;

    type Config = Cfg;
    type Service = ServiceFn<F, Fut, Req, Res, Err>;
    type InitError = ();
    type Future = Ready<Result<Self::Service, Self::InitError>>;

    fn new_service(&self, _: &Cfg) -> Self::Future {
        ok(ServiceFn::new(self.f.clone()))
    }
}

/// Convert `Fn(&Config) -> Future<Service>` fn to NewService
struct FnNewServiceConfig<F, Fut, Cfg, Srv, Err>
where
    F: Fn(&Cfg) -> Fut,
    Fut: Future<Output = Result<Srv, Err>>,
    Srv: Service,
{
    f: F,
    _t: PhantomData<(Fut, Cfg, Srv, Err)>,
}

impl<F, Fut, Cfg, Srv, Err> FnNewServiceConfig<F, Fut, Cfg, Srv, Err>
where
    F: Fn(&Cfg) -> Fut,
    Fut: Future<Output = Result<Srv, Err>>,
    Srv: Service,
{
    pub fn new(f: F) -> Self {
        FnNewServiceConfig { f, _t: PhantomData }
    }
}

impl<F, Fut, Cfg, Srv, Err> ServiceFactory for FnNewServiceConfig<F, Fut, Cfg, Srv, Err>
where
    F: Fn(&Cfg) -> Fut,
    Fut: Future<Output = Result<Srv, Err>>,
    Srv: Service,
{
    type Request = Srv::Request;
    type Response = Srv::Response;
    type Error = Srv::Error;

    type Config = Cfg;
    type Service = Srv;
    type InitError = Err;
    type Future = FnNewServiceConfigFut<Fut, Srv, Err>;

    fn new_service(&self, cfg: &Cfg) -> Self::Future {
        FnNewServiceConfigFut {
            fut: (self.f)(cfg),
            _t: PhantomData,
        }
    }
}

#[pin_project]
struct FnNewServiceConfigFut<R, S, E>
where
    R: Future<Output = Result<S, E>>,
    S: Service,
{
    #[pin]
    fut: R,
    _t: PhantomData<(S,)>,
}

impl<R, S, E> Future for FnNewServiceConfigFut<R, S, E>
where
    R: Future<Output = Result<S, E>>,
    S: Service,
{
    type Output = Result<S, E>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Poll::Ready(Ok(futures::ready!(self.project().fut.poll(cx))?))
    }
}

/// Converter for `Fn() -> Future<Service>` fn
pub struct FnNewServiceNoConfig<F, C, R, S, E>
where
    F: Fn() -> R,
    R: Future<Output = Result<S, E>>,
    S: Service,
{
    f: F,
    _t: PhantomData<C>,
}

impl<F, C, R, S, E> FnNewServiceNoConfig<F, C, R, S, E>
where
    F: Fn() -> R,
    R: Future<Output = Result<S, E>>,
    S: Service,
{
    fn new(f: F) -> Self {
        FnNewServiceNoConfig { f, _t: PhantomData }
    }
}

impl<F, C, R, S, E> ServiceFactory for FnNewServiceNoConfig<F, C, R, S, E>
where
    F: Fn() -> R,
    R: Future<Output = Result<S, E>>,
    S: Service,
{
    type Request = S::Request;
    type Response = S::Response;
    type Error = S::Error;
    type Service = S;
    type Config = C;
    type InitError = E;
    type Future = R;

    fn new_service(&self, _: &C) -> Self::Future {
        (self.f)()
    }
}

impl<F, C, R, S, E> Clone for FnNewServiceNoConfig<F, C, R, S, E>
where
    F: Fn() -> R + Clone,
    R: Future<Output = Result<S, E>>,
    S: Service,
{
    fn clone(&self) -> Self {
        Self::new(self.f.clone())
    }
}

impl<F, C, R, S, E> IntoServiceFactory<FnNewServiceNoConfig<F, C, R, S, E>> for F
where
    F: Fn() -> R,
    R: Future<Output = Result<S, E>>,
    S: Service,
{
    fn into_factory(self) -> FnNewServiceNoConfig<F, C, R, S, E> {
        FnNewServiceNoConfig::new(self)
    }
}
