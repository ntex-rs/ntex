use actix_service::{IntoNewService, IntoService, NewService, Service};
use futures::{try_ready, Async, Future, Poll};

use super::counter::{Counter, CounterGuard};

/// InFlight - new service for service that can limit number of in-flight
/// async requests.
///
/// Default number of in-flight requests is 15
pub struct InFlight<T> {
    factory: T,
    max_inflight: usize,
}

impl<T> InFlight<T> {
    pub fn new<F>(factory: F) -> Self
    where
        T: NewService,
        F: IntoNewService<T>,
    {
        Self {
            factory: factory.into_new_service(),
            max_inflight: 15,
        }
    }

    /// Set max number of in-flight requests.
    ///
    /// By default max in-flight requests is 15.
    pub fn max_inflight(mut self, max: usize) -> Self {
        self.max_inflight = max;
        self
    }
}

impl<T> NewService for InFlight<T>
where
    T: NewService,
{
    type Request = T::Request;
    type Response = T::Response;
    type Error = T::Error;
    type InitError = T::InitError;
    type Service = InFlightService<T::Service>;
    type Future = InFlightResponseFuture<T>;

    fn new_service(&self) -> Self::Future {
        InFlightResponseFuture {
            fut: self.factory.new_service(),
            max_inflight: self.max_inflight,
        }
    }
}

pub struct InFlightResponseFuture<T: NewService> {
    fut: T::Future,
    max_inflight: usize,
}

impl<T: NewService> Future for InFlightResponseFuture<T> {
    type Item = InFlightService<T::Service>;
    type Error = T::InitError;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        Ok(Async::Ready(InFlightService::with_max_inflight(
            self.max_inflight,
            try_ready!(self.fut.poll()),
        )))
    }
}

pub struct InFlightService<T> {
    service: T,
    count: Counter,
}

impl<T> InFlightService<T> {
    pub fn new<F>(service: F) -> Self
    where
        T: Service,
        F: IntoService<T>,
    {
        Self {
            service: service.into_service(),
            count: Counter::new(15),
        }
    }

    pub fn with_max_inflight<F>(max: usize, service: F) -> Self
    where
        T: Service,
        F: IntoService<T>,
    {
        Self {
            service: service.into_service(),
            count: Counter::new(max),
        }
    }
}

impl<T> Service for InFlightService<T>
where
    T: Service,
{
    type Request = T::Request;
    type Response = T::Response;
    type Error = T::Error;
    type Future = InFlightServiceResponse<T>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        let res = self.service.poll_ready()?;
        if res.is_ready() && !self.count.available() {
            log::trace!("InFlight limit exceeded");
            return Ok(Async::NotReady);
        }
        Ok(res)
    }

    fn call(&mut self, req: T::Request) -> Self::Future {
        InFlightServiceResponse {
            fut: self.service.call(req),
            _guard: self.count.get(),
        }
    }
}

#[doc(hidden)]
pub struct InFlightServiceResponse<T: Service> {
    fut: T::Future,
    _guard: CounterGuard,
}

impl<T: Service> Future for InFlightServiceResponse<T> {
    type Item = T::Response;
    type Error = T::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.fut.poll()
    }
}
