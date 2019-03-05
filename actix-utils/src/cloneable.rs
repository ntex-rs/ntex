use std::marker::PhantomData;
use std::rc::Rc;

use actix_service::Service;
use futures::Poll;

use super::cell::Cell;

/// Service that allows to turn non-clone service to a service with `Clone` impl
pub struct CloneableService<T: 'static> {
    service: Cell<T>,
    _t: PhantomData<Rc<()>>,
}

impl<T: 'static> CloneableService<T> {
    pub fn new<R>(service: T) -> Self
    where
        T: Service<R>,
    {
        Self {
            service: Cell::new(service),
            _t: PhantomData,
        }
    }
}

impl<T: 'static> Clone for CloneableService<T> {
    fn clone(&self) -> Self {
        Self {
            service: self.service.clone(),
            _t: PhantomData,
        }
    }
}

impl<T, R> Service<R> for CloneableService<T>
where
    T: Service<R> + 'static,
{
    type Response = T::Response;
    type Error = T::Error;
    type Future = T::Future;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.service.get_mut().poll_ready()
    }

    fn call(&mut self, req: R) -> Self::Future {
        self.service.get_mut().call(req)
    }
}
