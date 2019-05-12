use std::marker::PhantomData;

use futures::future::Future;
use futures::{try_ready, Async, IntoFuture, Poll};

use crate::cell::Cell;
use crate::{IntoService, NewService, Service};

/// Convert `Fn(&Config, &mut Service) -> Future<Service>` fn to a NewService
pub fn apply_cfg<F, C, T, R, S>(srv: T, f: F) -> ApplyConfigService<F, C, T, R, S>
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

/// Convert `Fn(&Config) -> Future<Service>` fn to NewService
pub struct ApplyConfigService<F, C, T, R, S>
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

pub struct FnNewServiceConfigFut<R, S>
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
