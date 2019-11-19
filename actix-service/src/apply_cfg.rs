use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::cell::Cell;
use crate::{Service, ServiceFactory};

/// Convert `Fn(&Config, &mut Service) -> Future<Service>` fn to a NewService
pub fn apply_cfg<F, C, T, R, S, E>(srv: T, f: F) -> ApplyConfigService<F, C, T, R, S, E>
where
    F: FnMut(&C, &mut T) -> R,
    T: Service,
    R: Future<Output = Result<S, E>>,
    S: Service,
{
    ApplyConfigService {
        f: Cell::new(f),
        srv: Cell::new(srv),
        _t: PhantomData,
    }
}

/// Convert `Fn(&Config, &mut Service) -> Future<Service>` fn to a NewService
/// Service get constructor from NewService.
pub fn apply_cfg_factory<F, C, T, R, S>(
    srv: T,
    f: F,
) -> ApplyConfigServiceFactory<F, C, T, R, S>
where
    C: Clone,
    F: FnMut(&C, &mut T::Service) -> R,
    T: ServiceFactory<Config = ()>,
    T::InitError: From<T::Error>,
    R: Future<Output = Result<S, T::InitError>>,
    S: Service,
{
    ApplyConfigServiceFactory {
        f: Cell::new(f),
        srv: Cell::new(srv),
        _t: PhantomData,
    }
}

/// Convert `Fn(&Config) -> Future<Service>` fn to NewService\
pub struct ApplyConfigService<F, C, T, R, S, E>
where
    F: FnMut(&C, &mut T) -> R,
    T: Service,
    R: Future<Output = Result<S, E>>,
    S: Service,
{
    f: Cell<F>,
    srv: Cell<T>,
    _t: PhantomData<(C, R, S)>,
}

impl<F, C, T, R, S, E> Clone for ApplyConfigService<F, C, T, R, S, E>
where
    F: FnMut(&C, &mut T) -> R,
    T: Service,
    R: Future<Output = Result<S, E>>,
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

impl<F, C, T, R, S, E> ServiceFactory for ApplyConfigService<F, C, T, R, S, E>
where
    F: FnMut(&C, &mut T) -> R,
    T: Service,
    R: Future<Output = Result<S, E>>,
    S: Service,
{
    type Config = C;
    type Request = S::Request;
    type Response = S::Response;
    type Error = S::Error;
    type Service = S;

    type InitError = E;
    type Future = R;

    fn new_service(&self, cfg: &C) -> Self::Future {
        unsafe { (self.f.get_mut_unsafe())(cfg, self.srv.get_mut_unsafe()) }
    }
}

/// Convert `Fn(&Config) -> Future<Service>` fn to NewService
pub struct ApplyConfigServiceFactory<F, C, T, R, S>
where
    C: Clone,
    F: FnMut(&C, &mut T::Service) -> R,
    T: ServiceFactory<Config = ()>,
    R: Future<Output = Result<S, T::InitError>>,
    S: Service,
{
    f: Cell<F>,
    srv: Cell<T>,
    _t: PhantomData<(C, R, S)>,
}

impl<F, C, T, R, S> Clone for ApplyConfigServiceFactory<F, C, T, R, S>
where
    C: Clone,
    F: FnMut(&C, &mut T::Service) -> R,
    T: ServiceFactory<Config = ()>,
    R: Future<Output = Result<S, T::InitError>>,
    S: Service,
{
    fn clone(&self) -> Self {
        Self {
            f: self.f.clone(),
            srv: self.srv.clone(),
            _t: PhantomData,
        }
    }
}

impl<F, C, T, R, S> ServiceFactory for ApplyConfigServiceFactory<F, C, T, R, S>
where
    C: Clone,
    F: FnMut(&C, &mut T::Service) -> R,
    T: ServiceFactory<Config = ()>,
    T::InitError: From<T::Error>,
    R: Future<Output = Result<S, T::InitError>>,
    S: Service,
{
    type Config = C;
    type Request = S::Request;
    type Response = S::Response;
    type Error = S::Error;
    type Service = S;

    type InitError = T::InitError;
    type Future = ApplyConfigServiceFactoryResponse<F, C, T, R, S>;

    fn new_service(&self, cfg: &C) -> Self::Future {
        ApplyConfigServiceFactoryResponse {
            f: self.f.clone(),
            cfg: cfg.clone(),
            fut: None,
            srv: None,
            srv_fut: Some(self.srv.get_ref().new_service(&())),
            _t: PhantomData,
        }
    }
}

#[pin_project::pin_project]
pub struct ApplyConfigServiceFactoryResponse<F, C, T, R, S>
where
    C: Clone,
    F: FnMut(&C, &mut T::Service) -> R,
    T: ServiceFactory<Config = ()>,
    T::InitError: From<T::Error>,
    R: Future<Output = Result<S, T::InitError>>,
    S: Service,
{
    cfg: C,
    f: Cell<F>,
    srv: Option<T::Service>,
    #[pin]
    srv_fut: Option<T::Future>,
    #[pin]
    fut: Option<R>,
    _t: PhantomData<(S,)>,
}

impl<F, C, T, R, S> Future for ApplyConfigServiceFactoryResponse<F, C, T, R, S>
where
    C: Clone,
    F: FnMut(&C, &mut T::Service) -> R,
    T: ServiceFactory<Config = ()>,
    T::InitError: From<T::Error>,
    R: Future<Output = Result<S, T::InitError>>,
    S: Service,
{
    type Output = Result<S, T::InitError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.as_mut().project();

        loop {
            if let Some(fut) = this.srv_fut.as_pin_mut() {
                match fut.poll(cx)? {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(srv) => {
                        this = self.as_mut().project();
                        this.srv_fut.set(None);
                        *this.srv = Some(srv);
                        continue;
                    }
                }
            }

            if let Some(fut) = this.fut.as_pin_mut() {
                return fut.poll(cx);
            } else if let Some(srv) = this.srv {
                match srv.poll_ready(cx)? {
                    Poll::Ready(_) => {
                        let fut = this.f.get_mut()(&this.cfg, srv);
                        this = self.as_mut().project();
                        this.fut.set(Some(fut));
                        continue;
                    }
                    Poll::Pending => return Poll::Pending,
                }
            } else {
                return Poll::Pending;
            }
        }
    }
}
