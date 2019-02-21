use actix_service::{NewService, Service};
use futures::{Future, Poll};

pub type BoxedService<Req, Res, Err> = Box<
    Service<
        Request = Req,
        Response = Res,
        Error = Err,
        Future = Box<Future<Item = Res, Error = Err>>,
    >,
>;

pub type BoxedNewService<Req, Res, Err, InitErr> = Box<
    NewService<
        Request = Req,
        Response = Res,
        Error = Err,
        InitError = InitErr,
        Service = BoxedService<Req, Res, Err>,
        Future = Box<Future<Item = BoxedService<Req, Res, Err>, Error = InitErr>>,
    >,
>;

/// Create boxed new service
pub fn new_service<T>(
    service: T,
) -> BoxedNewService<T::Request, T::Response, T::Error, T::InitError>
where
    T: NewService + 'static,
    T::Service: 'static,
{
    Box::new(NewServiceWrapper(service))
}

/// Create boxed service
pub fn service<T>(service: T) -> BoxedService<T::Request, T::Response, T::Error>
where
    T: Service + 'static,
    T::Future: 'static,
{
    Box::new(ServiceWrapper(service))
}

struct NewServiceWrapper<T: NewService>(T);

impl<T, Req, Res, Err, InitErr> NewService for NewServiceWrapper<T>
where
    Req: 'static,
    Res: 'static,
    Err: 'static,
    InitErr: 'static,
    T: NewService<Request = Req, Response = Res, Error = Err, InitError = InitErr>,
    T::Future: 'static,
    T::Service: 'static,
    <T::Service as Service>::Future: 'static,
{
    type Request = Req;
    type Response = Res;
    type Error = Err;
    type InitError = InitErr;
    type Service = BoxedService<Req, Res, Err>;
    type Future = Box<Future<Item = Self::Service, Error = Self::InitError>>;

    fn new_service(&self) -> Self::Future {
        Box::new(
            self.0
                .new_service()
                .map(|service| ServiceWrapper::boxed(service)),
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
    type Future = Box<Future<Item = Self::Response, Error = Self::Error>>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.0.poll_ready()
    }

    fn call(&mut self, req: Self::Request) -> Self::Future {
        Box::new(self.0.call(req))
    }
}
