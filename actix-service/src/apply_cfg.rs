use std::marker::PhantomData;

use futures::future::Future;
use futures::{try_ready, Async, IntoFuture, Poll};

use crate::cell::Cell;
use crate::{IntoService, NewService, Service};

/// Convert `Fn(&Config, &mut Service) -> Future<Service>` fn to a NewService
pub fn apply_cfg<F, C, T, R, S>(
    srv: T,
    f: F,
) -> impl NewService<
    Config = C,
    Request = S::Request,
    Response = S::Response,
    Error = S::Error,
    Service = S,
    InitError = R::Error,
> + Clone
where
    F: FnMut(&C, &mut T) -> R,
    T: Service,
    R: IntoFuture,
    R::Item: IntoService<S>,
    S: Service,
{
    ApplyConfigService {
        f: Cell::new(f),
        srv: Cell::new(srv.into_service()),
        _t: PhantomData,
    }
}

/// Convert `Fn(&Config, &mut Service) -> Future<Service>` fn to a NewService
/// Service get constructor from NewService.
pub fn new_apply_cfg<F, C, T, R, S>(
    srv: T,
    f: F,
) -> impl NewService<
    Config = C,
    Request = S::Request,
    Response = S::Response,
    Error = S::Error,
    Service = S,
    InitError = T::InitError,
> + Clone
where
    C: Clone,
    F: FnMut(&C, &mut T::Service) -> R,
    T: NewService<Config = ()>,
    T::InitError: From<T::Error>,
    R: IntoFuture<Error = T::InitError>,
    R::Item: IntoService<S>,
    S: Service,
{
    ApplyConfigNewService {
        f: Cell::new(f),
        srv: Cell::new(srv),
        _t: PhantomData,
    }
}

/// Convert `Fn(&Config) -> Future<Service>` fn to NewService
struct ApplyConfigService<F, C, T, R, S>
where
    F: FnMut(&C, &mut T) -> R,
    T: Service,
    R: IntoFuture,
    R::Item: IntoService<S>,
    S: Service,
{
    f: Cell<F>,
    srv: Cell<T>,
    _t: PhantomData<(C, R, S)>,
}

impl<F, C, T, R, S> Clone for ApplyConfigService<F, C, T, R, S>
where
    F: FnMut(&C, &mut T) -> R,
    T: Service,
    R: IntoFuture,
    R::Item: IntoService<S>,
    S: Service,
{
    fn clone(&self) -> Self {
        ApplyConfigService {
            f: self.f.clone(),
            srv: self.srv.clone(),
            _t: PhantomData,
        }
    }
}

impl<F, C, T, R, S> NewService for ApplyConfigService<F, C, T, R, S>
where
    F: FnMut(&C, &mut T) -> R,
    T: Service,
    R: IntoFuture,
    R::Item: IntoService<S>,
    S: Service,
{
    type Config = C;
    type Request = S::Request;
    type Response = S::Response;
    type Error = S::Error;
    type Service = S;

    type InitError = R::Error;
    type Future = FnNewServiceConfigFut<R, S>;

    fn new_service(&self, cfg: &C) -> Self::Future {
        FnNewServiceConfigFut {
            fut: unsafe { (self.f.get_mut_unsafe())(cfg, self.srv.get_mut_unsafe()) }
                .into_future(),
            _t: PhantomData,
        }
    }
}

struct FnNewServiceConfigFut<R, S>
where
    R: IntoFuture,
    R::Item: IntoService<S>,
    S: Service,
{
    fut: R::Future,
    _t: PhantomData<(S,)>,
}

impl<R, S> Future for FnNewServiceConfigFut<R, S>
where
    R: IntoFuture,
    R::Item: IntoService<S>,
    S: Service,
{
    type Item = S;
    type Error = R::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        Ok(Async::Ready(try_ready!(self.fut.poll()).into_service()))
    }
}

/// Convert `Fn(&Config) -> Future<Service>` fn to NewService
struct ApplyConfigNewService<F, C, T, R, S>
where
    C: Clone,
    F: FnMut(&C, &mut T::Service) -> R,
    T: NewService<Config = ()>,
    R: IntoFuture<Error = T::InitError>,
    R::Item: IntoService<S>,
    S: Service,
{
    f: Cell<F>,
    srv: Cell<T>,
    _t: PhantomData<(C, R, S)>,
}

impl<F, C, T, R, S> Clone for ApplyConfigNewService<F, C, T, R, S>
where
    C: Clone,
    F: FnMut(&C, &mut T::Service) -> R,
    T: NewService<Config = ()>,
    R: IntoFuture<Error = T::InitError>,
    R::Item: IntoService<S>,
    S: Service,
{
    fn clone(&self) -> Self {
        ApplyConfigNewService {
            f: self.f.clone(),
            srv: self.srv.clone(),
            _t: PhantomData,
        }
    }
}

impl<F, C, T, R, S> NewService for ApplyConfigNewService<F, C, T, R, S>
where
    C: Clone,
    F: FnMut(&C, &mut T::Service) -> R,
    T: NewService<Config = ()>,
    T::InitError: From<T::Error>,
    R: IntoFuture<Error = T::InitError>,
    R::Item: IntoService<S>,
    S: Service,
{
    type Config = C;
    type Request = S::Request;
    type Response = S::Response;
    type Error = S::Error;
    type Service = S;

    type InitError = R::Error;
    type Future = ApplyConfigNewServiceFut<F, C, T, R, S>;

    fn new_service(&self, cfg: &C) -> Self::Future {
        ApplyConfigNewServiceFut {
            f: self.f.clone(),
            cfg: cfg.clone(),
            fut: None,
            srv: None,
            srv_fut: Some(self.srv.get_ref().new_service(&())),
            _t: PhantomData,
        }
    }
}

struct ApplyConfigNewServiceFut<F, C, T, R, S>
where
    C: Clone,
    F: FnMut(&C, &mut T::Service) -> R,
    T: NewService<Config = ()>,
    T::InitError: From<T::Error>,
    R: IntoFuture<Error = T::InitError>,
    R::Item: IntoService<S>,
    S: Service,
{
    cfg: C,
    f: Cell<F>,
    srv: Option<T::Service>,
    srv_fut: Option<T::Future>,
    fut: Option<R::Future>,
    _t: PhantomData<(S,)>,
}

impl<F, C, T, R, S> Future for ApplyConfigNewServiceFut<F, C, T, R, S>
where
    C: Clone,
    F: FnMut(&C, &mut T::Service) -> R,
    T: NewService<Config = ()>,
    T::InitError: From<T::Error>,
    R: IntoFuture<Error = T::InitError>,
    R::Item: IntoService<S>,
    S: Service,
{
    type Item = S;
    type Error = R::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        if let Some(ref mut fut) = self.srv_fut {
            match fut.poll()? {
                Async::NotReady => return Ok(Async::NotReady),
                Async::Ready(srv) => {
                    let _ = self.srv_fut.take();
                    self.srv = Some(srv);
                    return self.poll();
                }
            }
        }

        if let Some(ref mut fut) = self.fut {
            Ok(Async::Ready(try_ready!(fut.poll()).into_service()))
        } else if let Some(ref mut srv) = self.srv {
            match srv.poll_ready()? {
                Async::NotReady => Ok(Async::NotReady),
                Async::Ready(_) => {
                    self.fut = Some(self.f.get_mut()(&self.cfg, srv).into_future());
                    return self.poll();
                }
            }
        } else {
            Ok(Async::NotReady)
        }
    }
}
