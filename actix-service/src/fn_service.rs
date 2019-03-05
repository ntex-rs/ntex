use std::marker::PhantomData;

use futures::future::{ok, FutureResult};
use futures::{Async, IntoFuture, Poll};

use crate::{IntoConfigurableNewService, IntoNewService, IntoService, NewService, Service};

/// Create `NewService` for function that can act as Service
pub fn fn_service<F, Req, Out, Cfg>(f: F) -> FnNewService<F, Req, Out, Cfg>
where
    F: FnMut(Req) -> Out + Clone,
    Out: IntoFuture,
{
    FnNewService::new(f)
}

/// Create `NewService` for function that can produce services
pub fn fn_factory<F, R, S, E, Req>(f: F) -> FnNewServiceNoConfig<F, R, S, E, Req>
where
    F: Fn() -> R,
    R: IntoFuture<Item = S, Error = E>,
    S: Service<Req>,
{
    FnNewServiceNoConfig::new(f)
}

/// Create `NewService` for function that can produce services with configuration
pub fn fn_cfg_factory<F, C, R, S, E, Req>(f: F) -> FnNewServiceConfig<F, C, R, S, E, Req>
where
    F: Fn(&C) -> R,
    R: IntoFuture<Item = S, Error = E>,
    S: Service<Req>,
{
    FnNewServiceConfig::new(f)
}

pub struct FnService<F, Req, Out>
where
    F: FnMut(Req) -> Out,
    Out: IntoFuture,
{
    f: F,
    _t: PhantomData<(Req,)>,
}

impl<F, Req, Out> FnService<F, Req, Out>
where
    F: FnMut(Req) -> Out,
    Out: IntoFuture,
{
    pub fn new(f: F) -> Self {
        FnService { f, _t: PhantomData }
    }
}

impl<F, Req, Out> Clone for FnService<F, Req, Out>
where
    F: FnMut(Req) -> Out + Clone,
    Out: IntoFuture,
{
    fn clone(&self) -> Self {
        FnService {
            f: self.f.clone(),
            _t: PhantomData,
        }
    }
}

impl<F, Req, Out> Service<Req> for FnService<F, Req, Out>
where
    F: FnMut(Req) -> Out,
    Out: IntoFuture,
{
    type Response = Out::Item;
    type Error = Out::Error;
    type Future = Out::Future;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(Async::Ready(()))
    }

    fn call(&mut self, req: Req) -> Self::Future {
        (self.f)(req).into_future()
    }
}

impl<F, Req, Out> IntoService<FnService<F, Req, Out>, Req> for F
where
    F: FnMut(Req) -> Out + 'static,
    Out: IntoFuture,
{
    fn into_service(self) -> FnService<F, Req, Out> {
        FnService::new(self)
    }
}

pub struct FnNewService<F, Req, Out, Cfg>
where
    F: FnMut(Req) -> Out,
    Out: IntoFuture,
{
    f: F,
    _t: PhantomData<(Req, Cfg)>,
}

impl<F, Req, Out, Cfg> FnNewService<F, Req, Out, Cfg>
where
    F: FnMut(Req) -> Out + Clone,
    Out: IntoFuture,
{
    pub fn new(f: F) -> Self {
        FnNewService { f, _t: PhantomData }
    }
}

impl<F, Req, Out, Cfg> NewService<Req, Cfg> for FnNewService<F, Req, Out, Cfg>
where
    F: FnMut(Req) -> Out + Clone,
    Out: IntoFuture,
{
    type Response = Out::Item;
    type Error = Out::Error;
    type Service = FnService<F, Req, Out>;

    type InitError = ();
    type Future = FutureResult<Self::Service, Self::InitError>;

    fn new_service(&self, _: &Cfg) -> Self::Future {
        ok(FnService::new(self.f.clone()))
    }
}

impl<F, Req, Out, Cfg> Clone for FnNewService<F, Req, Out, Cfg>
where
    F: FnMut(Req) -> Out + Clone,
    Out: IntoFuture,
{
    fn clone(&self) -> Self {
        Self::new(self.f.clone())
    }
}

impl<F, Req, Out, Cfg> IntoNewService<FnNewService<F, Req, Out, Cfg>, Req, Cfg> for F
where
    F: Fn(Req) -> Out + Clone,
    Out: IntoFuture,
{
    fn into_new_service(self) -> FnNewService<F, Req, Out, Cfg> {
        FnNewService::new(self)
    }
}

/// Converter for `Fn() -> Future<Service>` fn
pub struct FnNewServiceNoConfig<F, R, S, E, Req>
where
    F: Fn() -> R,
    R: IntoFuture<Item = S, Error = E>,
    S: Service<Req>,
{
    f: F,
    _t: PhantomData<Req>,
}

impl<F, R, S, E, Req> FnNewServiceNoConfig<F, R, S, E, Req>
where
    F: Fn() -> R,
    R: IntoFuture<Item = S, Error = E>,
    S: Service<Req>,
{
    pub fn new(f: F) -> Self {
        FnNewServiceNoConfig { f, _t: PhantomData }
    }
}

impl<F, R, S, E, Req> NewService<Req, ()> for FnNewServiceNoConfig<F, R, S, E, Req>
where
    F: Fn() -> R,
    R: IntoFuture<Item = S, Error = E>,
    S: Service<Req>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Service = S;

    type InitError = E;
    type Future = R::Future;

    fn new_service(&self, _: &()) -> Self::Future {
        (self.f)().into_future()
    }
}

impl<F, R, S, E, Req> Clone for FnNewServiceNoConfig<F, R, S, E, Req>
where
    F: Fn() -> R + Clone,
    R: IntoFuture<Item = S, Error = E>,
    S: Service<Req>,
{
    fn clone(&self) -> Self {
        Self::new(self.f.clone())
    }
}

impl<F, R, S, E, Req> IntoNewService<FnNewServiceNoConfig<F, R, S, E, Req>, Req, ()> for F
where
    F: Fn() -> R,
    R: IntoFuture<Item = S, Error = E>,
    S: Service<Req>,
{
    fn into_new_service(self) -> FnNewServiceNoConfig<F, R, S, E, Req> {
        FnNewServiceNoConfig::new(self)
    }
}

/// Convert `Fn(&Config) -> Future<Service>` fn to NewService
pub struct FnNewServiceConfig<F, C, R, S, E, Req>
where
    F: Fn(&C) -> R,
    R: IntoFuture<Item = S, Error = E>,
    S: Service<Req>,
{
    f: F,
    _t: PhantomData<(C, R, S, E, Req)>,
}

impl<F, C, R, S, E, Req> FnNewServiceConfig<F, C, R, S, E, Req>
where
    F: Fn(&C) -> R,
    R: IntoFuture<Item = S, Error = E>,
    S: Service<Req>,
{
    pub fn new(f: F) -> Self {
        FnNewServiceConfig { f, _t: PhantomData }
    }
}

impl<F, C, R, S, E, Req> NewService<Req, C> for FnNewServiceConfig<F, C, R, S, E, Req>
where
    F: Fn(&C) -> R,
    R: IntoFuture<Item = S, Error = E>,
    S: Service<Req>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Service = S;

    type InitError = E;
    type Future = R::Future;

    fn new_service(&self, cfg: &C) -> Self::Future {
        (self.f)(cfg).into_future()
    }
}

impl<F, C, R, S, E, Req> Clone for FnNewServiceConfig<F, C, R, S, E, Req>
where
    F: Fn(&C) -> R + Clone,
    R: IntoFuture<Item = S, Error = E>,
    S: Service<Req>,
{
    fn clone(&self) -> Self {
        Self::new(self.f.clone())
    }
}

impl<F, C, R, S, E, Req>
    IntoConfigurableNewService<FnNewServiceConfig<F, C, R, S, E, Req>, Req, C> for F
where
    F: Fn(&C) -> R,
    R: IntoFuture<Item = S, Error = E>,
    S: Service<Req>,
{
    fn into_new_service(self) -> FnNewServiceConfig<F, C, R, S, E, Req> {
        FnNewServiceConfig::new(self)
    }
}

#[cfg(test)]
mod tests {
    use futures::Future;

    use super::*;
    use crate::{IntoService, NewService, Service, ServiceExt};

    #[test]
    fn test_fn_service() {
        let mut rt = actix_rt::Runtime::new().unwrap();

        let srv = (|t: &str| -> Result<usize, ()> { Ok(1) }).into_service();
        let mut srv = srv.and_then(|test: usize| Ok(test));

        let s = "HELLO".to_owned();
        let res = rt.block_on(srv.call(&s)).unwrap();
        assert_eq!(res, 1);
    }
}
