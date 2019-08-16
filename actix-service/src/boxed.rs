use futures::future::{err, ok, Either, FutureResult};
use futures::{Async, Future, IntoFuture, Poll};

use crate::{NewService, Service};

pub type BoxedService<Req, Res, Err> = Box<
    dyn Service<
        Request = Req,
        Response = Res,
        Error = Err,
        Future = BoxedServiceResponse<Res, Err>,
    >,
>;

pub type BoxedServiceResponse<Res, Err> =
    Either<FutureResult<Res, Err>, Box<dyn Future<Item = Res, Error = Err>>>;

pub struct BoxedNewService<C, Req, Res, Err, InitErr>(Inner<C, Req, Res, Err, InitErr>);

/// Create boxed new service
pub fn new_service<T>(
    service: T,
) -> BoxedNewService<T::Config, T::Request, T::Response, T::Error, T::InitError>
where
    T: NewService + 'static,
    T::Request: 'static,
    T::Response: 'static,
    T::Service: 'static,
    T::Future: 'static,
    T::Error: 'static,
    T::InitError: 'static,
{
    BoxedNewService(Box::new(NewServiceWrapper {
        service,
        _t: std::marker::PhantomData,
    }))
}

/// Create boxed service
pub fn service<T>(service: T) -> BoxedService<T::Request, T::Response, T::Error>
where
    T: Service + 'static,
    T::Future: 'static,
{
    Box::new(ServiceWrapper(service))
}

type Inner<C, Req, Res, Err, InitErr> = Box<
    dyn NewService<
        Config = C,
        Request = Req,
        Response = Res,
        Error = Err,
        InitError = InitErr,
        Service = BoxedService<Req, Res, Err>,
        Future = Box<dyn Future<Item = BoxedService<Req, Res, Err>, Error = InitErr>>,
    >,
>;

impl<C, Req, Res, Err, InitErr> NewService for BoxedNewService<C, Req, Res, Err, InitErr>
where
    Req: 'static,
    Res: 'static,
    Err: 'static,
    InitErr: 'static,
{
    type Request = Req;
    type Response = Res;
    type Error = Err;
    type InitError = InitErr;
    type Config = C;
    type Service = BoxedService<Req, Res, Err>;
    type Future = Box<dyn Future<Item = Self::Service, Error = Self::InitError>>;

    fn new_service(&self, cfg: &C) -> Self::Future {
        self.0.new_service(cfg)
    }
}

struct NewServiceWrapper<C, T: NewService> {
    service: T,
    _t: std::marker::PhantomData<C>,
}

impl<C, T, Req, Res, Err, InitErr> NewService for NewServiceWrapper<C, T>
where
    Req: 'static,
    Res: 'static,
    Err: 'static,
    InitErr: 'static,
    T: NewService<Config = C, Request = Req, Response = Res, Error = Err, InitError = InitErr>,
    T::Future: 'static,
    T::Service: 'static,
    <T::Service as Service>::Future: 'static,
{
    type Request = Req;
    type Response = Res;
    type Error = Err;
    type InitError = InitErr;
    type Config = C;
    type Service = BoxedService<Req, Res, Err>;
    type Future = Box<dyn Future<Item = Self::Service, Error = Self::InitError>>;

    fn new_service(&self, cfg: &C) -> Self::Future {
        Box::new(
            self.service
                .new_service(cfg)
                .into_future()
                .map(ServiceWrapper::boxed),
        )
    }
}

struct ServiceWrapper<T: Service>(T);

impl<T> ServiceWrapper<T>
where
    T: Service + 'static,
    T::Future: 'static,
{
    fn boxed(service: T) -> BoxedService<T::Request, T::Response, T::Error> {
        Box::new(ServiceWrapper(service))
    }
}

impl<T, Req, Res, Err> Service for ServiceWrapper<T>
where
    T: Service<Request = Req, Response = Res, Error = Err>,
    T::Future: 'static,
{
    type Request = Req;
    type Response = Res;
    type Error = Err;
    type Future = Either<
        FutureResult<Self::Response, Self::Error>,
        Box<dyn Future<Item = Self::Response, Error = Self::Error>>,
    >;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.0.poll_ready()
    }

    fn call(&mut self, req: Self::Request) -> Self::Future {
        let mut fut = self.0.call(req);
        match fut.poll() {
            Ok(Async::Ready(res)) => Either::A(ok(res)),
            Err(e) => Either::A(err(e)),
            Ok(Async::NotReady) => Either::B(Box::new(fut)),
        }
    }
}
