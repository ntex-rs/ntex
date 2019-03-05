use std::marker::PhantomData;

use crate::{NewService, Service};
use futures::{Future, IntoFuture, Poll};

pub type BoxedService<Req, Res, Err> = Box<
    Service<Req, Response = Res, Error = Err, Future = Box<Future<Item = Res, Error = Err>>>,
>;

/// Create boxed new service
pub fn new_service<T, R, C>(
    service: T,
) -> BoxedNewService<C, R, T::Response, T::Error, T::InitError>
where
    C: 'static,
    T: NewService<R, C> + 'static,
    T::Response: 'static,
    T::Service: 'static,
    T::Future: 'static,
    T::Error: 'static,
    T::InitError: 'static,
    R: 'static,
{
    BoxedNewService(Box::new(NewServiceWrapper {
        service,
        _t: PhantomData,
    }))
}

/// Create boxed service
pub fn service<T, R>(service: T) -> BoxedService<R, T::Response, T::Error>
where
    T: Service<R> + 'static,
    T::Future: 'static,
    R: 'static,
{
    Box::new(ServiceWrapper {
        service,
        _t: PhantomData,
    })
}

type Inner<C, Req, Res, Err, InitErr> = Box<
    NewService<
        Req,
        C,
        Response = Res,
        Error = Err,
        InitError = InitErr,
        Service = BoxedService<Req, Res, Err>,
        Future = Box<Future<Item = BoxedService<Req, Res, Err>, Error = InitErr>>,
    >,
>;

pub struct BoxedNewService<C, Req, Res, Err, InitErr>(Inner<C, Req, Res, Err, InitErr>);

impl<C, Req, Res, Err, InitErr> NewService<Req, C>
    for BoxedNewService<C, Req, Res, Err, InitErr>
where
    Req: 'static,
    Res: 'static,
    Err: 'static,
    InitErr: 'static,
{
    type Response = Res;
    type Error = Err;
    type InitError = InitErr;
    type Service = BoxedService<Req, Res, Err>;
    type Future = Box<Future<Item = Self::Service, Error = Self::InitError>>;

    fn new_service(&self, cfg: &C) -> Self::Future {
        self.0.new_service(cfg)
    }
}

struct NewServiceWrapper<T: NewService<R, C>, R, C> {
    service: T,
    _t: std::marker::PhantomData<(R, C)>,
}

impl<C, T, Req, Res, Err, InitErr> NewService<Req, C> for NewServiceWrapper<T, Req, C>
where
    Req: 'static,
    Res: 'static,
    Err: 'static,
    InitErr: 'static,
    T: NewService<Req, C, Response = Res, Error = Err, InitError = InitErr>,
    T::Future: 'static,
    T::Service: 'static,
    <T::Service as Service<Req>>::Future: 'static,
{
    type Response = Res;
    type Error = Err;
    type InitError = InitErr;
    type Service = BoxedService<Req, Res, Err>;
    type Future = Box<Future<Item = Self::Service, Error = Self::InitError>>;

    fn new_service(&self, cfg: &C) -> Self::Future {
        Box::new(
            self.service
                .new_service(cfg)
                .into_future()
                .map(ServiceWrapper::boxed),
        )
    }
}

struct ServiceWrapper<T: Service<R>, R> {
    service: T,
    _t: PhantomData<R>,
}

impl<T, R> ServiceWrapper<T, R>
where
    T: Service<R> + 'static,
    T::Future: 'static,
    R: 'static,
{
    fn boxed(service: T) -> BoxedService<R, T::Response, T::Error> {
        Box::new(ServiceWrapper {
            service,
            _t: PhantomData,
        })
    }
}

impl<T, Req, Res, Err> Service<Req> for ServiceWrapper<T, Req>
where
    T: Service<Req, Response = Res, Error = Err>,
    T::Future: 'static,
    Req: 'static,
{
    type Response = Res;
    type Error = Err;
    type Future = Box<Future<Item = Self::Response, Error = Self::Error>>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.service.poll_ready()
    }

    fn call(&mut self, req: Req) -> Self::Future {
        Box::new(self.service.call(req))
    }
}
