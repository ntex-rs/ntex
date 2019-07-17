#![allow(deprecated)]
use std::marker::PhantomData;
use std::rc::Rc;

use actix_service::Service;
use futures::Poll;

use super::cell::Cell;

#[doc(hidden)]
#[deprecated(since = "0.4.3", note = "support will be removed in actix-utils 0.4.5")]
/// Service that allows to turn non-clone service to a service with `Clone` impl
pub struct CloneableService<T> {
    service: Cell<T>,
    _t: PhantomData<Rc<()>>,
}

impl<T> CloneableService<T> {
    pub fn new(service: T) -> Self
    where
        T: Service,
    {
        Self {
            service: Cell::new(service),
            _t: PhantomData,
        }
    }
}

impl<T> Clone for CloneableService<T> {
    fn clone(&self) -> Self {
        Self {
            service: self.service.clone(),
            _t: PhantomData,
        }
    }
}

impl<T> Service for CloneableService<T>
where
    T: Service,
{
    type Request = T::Request;
    type Response = T::Response;
    type Error = T::Error;
    type Future = T::Future;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.service.get_mut().poll_ready()
    }

    fn call(&mut self, req: T::Request) -> Self::Future {
        self.service.get_mut().call(req)
    }
}
