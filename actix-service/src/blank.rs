use std::marker::PhantomData;

use futures::future::{ok, FutureResult};
use futures::{Async, Poll};

use super::{NewService, Service};

/// Empty service
#[derive(Clone)]
pub struct Blank<R, E> {
    _t: PhantomData<(R, E)>,
}

impl<R, E> Blank<R, E> {
    //pub fn new() -> Blank<R, E> {
    //    Blank { _t: PhantomData }
    //}

    pub fn err<E1>(self) -> Blank<R, E1> {
        Blank { _t: PhantomData }
    }
}

impl<R> Blank<R, ()> {
    pub fn new<E>() -> Blank<R, E> {
        Blank { _t: PhantomData }
    }
}

impl<R, E> Default for Blank<R, E> {
    fn default() -> Blank<R, E> {
        Blank { _t: PhantomData }
    }
}

impl<R, E> Service for Blank<R, E> {
    type Request = R;
    type Response = R;
    type Error = E;
    type Future = FutureResult<R, E>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(Async::Ready(()))
    }

    fn call(&mut self, req: R) -> Self::Future {
        ok(req)
    }
}

/// Empty service factory
pub struct BlankNewService<R, E1, E2 = ()> {
    _t: PhantomData<(R, E1, E2)>,
}

impl<R, E1, E2> BlankNewService<R, E1, E2> {
    pub fn new() -> BlankNewService<R, E1, E2> {
        BlankNewService { _t: PhantomData }
    }
}

impl<R, E1> BlankNewService<R, E1, ()> {
    pub fn new_unit() -> BlankNewService<R, E1, ()> {
        BlankNewService { _t: PhantomData }
    }
}

impl<R, E1, E2> Default for BlankNewService<R, E1, E2> {
    fn default() -> BlankNewService<R, E1, E2> {
        Self::new()
    }
}

impl<R, E1, E2> NewService for BlankNewService<R, E1, E2> {
    type Request = R;
    type Response = R;
    type Error = E1;
    type InitError = E2;
    type Service = Blank<R, E1>;
    type Future = FutureResult<Self::Service, Self::InitError>;

    fn new_service(&self) -> Self::Future {
        ok(Blank::default())
    }
}
