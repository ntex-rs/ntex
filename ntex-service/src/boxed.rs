use std::{
    future::Future, marker::PhantomData, pin::Pin, rc::Rc, task::Context, task::Poll,
};

use crate::{Service, ServiceFactory};

pub type BoxFuture<I, E> = Pin<Box<dyn Future<Output = Result<I, E>>>>;

pub type BoxService<Req, Res, Err> =
    Box<dyn Service<Req, Response = Res, Error = Err, Future = BoxFuture<Res, Err>>>;

pub type RcService<Req, Res, Err> =
    Rc<dyn Service<Req, Response = Res, Error = Err, Future = BoxFuture<Res, Err>>>;

pub struct BoxServiceFactory<C, Req, Res, Err, InitErr>(
    Inner<C, Req, Res, Err, InitErr>,
);

/// Create boxed service factory
pub fn factory<T, R>(
    factory: T,
) -> BoxServiceFactory<T::Config, R, T::Response, T::Error, T::InitError>
where
    R: 'static,
    T: ServiceFactory<R> + 'static,
    T::Response: 'static,
    T::Error: 'static,
    T::InitError: 'static,
{
    BoxServiceFactory(Box::new(FactoryWrapper {
        factory,
        _t: std::marker::PhantomData,
    }))
}

/// Create boxed service
pub fn service<T, R>(service: T) -> BoxService<R, T::Response, T::Error>
where
    R: 'static,
    T: Service<R> + 'static,
    T::Future: 'static,
{
    Box::new(ServiceWrapper(service, PhantomData))
}

/// Create rc service
pub fn rcservice<T, R>(service: T) -> RcService<R, T::Response, T::Error>
where
    R: 'static,
    T: Service<R> + 'static,
    T::Future: 'static,
{
    Rc::new(ServiceWrapper(service, PhantomData))
}

type Inner<C, Req, Res, Err, InitErr> = Box<
    dyn ServiceFactory<
        Req,
        Config = C,
        Response = Res,
        Error = Err,
        InitError = InitErr,
        Service = BoxService<Req, Res, Err>,
        Future = BoxFuture<BoxService<Req, Res, Err>, InitErr>,
    >,
>;

impl<C, Req, Res, Err, InitErr> ServiceFactory<Req>
    for BoxServiceFactory<C, Req, Res, Err, InitErr>
where
    Req: 'static,
    Res: 'static,
    Err: 'static,
    InitErr: 'static,
{
    type Response = Res;
    type Error = Err;
    type InitError = InitErr;
    type Config = C;
    type Service = BoxService<Req, Res, Err>;

    type Future = BoxFuture<Self::Service, InitErr>;

    fn new_service(&self, cfg: C) -> Self::Future {
        self.0.new_service(cfg)
    }
}

struct FactoryWrapper<C, R, T: ServiceFactory<R>> {
    factory: T,
    _t: std::marker::PhantomData<(C, R)>,
}

impl<C, R, T, Res, Err, InitErr> ServiceFactory<R> for FactoryWrapper<C, R, T>
where
    R: 'static,
    Res: 'static,
    Err: 'static,
    InitErr: 'static,
    T: ServiceFactory<R, Config = C, Response = Res, Error = Err, InitError = InitErr>
        + 'static,
{
    type Response = Res;
    type Error = Err;
    type InitError = InitErr;
    type Config = C;
    type Service = BoxService<R, Res, Err>;
    type Future = BoxFuture<Self::Service, Self::InitError>;

    fn new_service(&self, cfg: C) -> Self::Future {
        let fut = self.factory.new_service(cfg);
        Box::pin(async move {
            let srv = fut.await?;
            Ok(ServiceWrapper::boxed(srv))
        })
    }
}

struct ServiceWrapper<T: Service<R>, R>(T, PhantomData<R>);

impl<T, R> ServiceWrapper<T, R>
where
    R: 'static,
    T: Service<R> + 'static,
    T::Future: 'static,
{
    fn boxed(service: T) -> BoxService<R, T::Response, T::Error> {
        Box::new(ServiceWrapper(service, PhantomData))
    }
}

impl<T, R, Res, Err> Service<R> for ServiceWrapper<T, R>
where
    T: Service<R, Response = Res, Error = Err>,
    T::Future: 'static,
{
    type Response = Res;
    type Error = Err;
    type Future = BoxFuture<Res, Err>;

    #[inline]
    fn poll_ready(&self, ctx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.0.poll_ready(ctx)
    }

    #[inline]
    fn shutdown(&self) {
        self.0.shutdown()
    }

    #[inline]
    fn call(&self, req: R) -> Self::Future {
        Box::pin(self.0.call(req))
    }
}
