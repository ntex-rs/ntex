use std::marker::PhantomData;

use futures::future::{ok, FutureResult};
use futures::{Async, IntoFuture, Poll};

use crate::{IntoNewTransform, IntoTransform, NewTransform, Transform};

pub struct FnTransform<F, S, Req, Res>
where
    F: FnMut(Req, &mut S) -> Res,
    Res: IntoFuture,
{
    f: F,
    _t: PhantomData<(S, Req, Res)>,
}

impl<F, S, Req, Res> FnTransform<F, S, Req, Res>
where
    F: FnMut(Req, &mut S) -> Res,
    Res: IntoFuture,
{
    pub fn new(f: F) -> Self {
        FnTransform { f, _t: PhantomData }
    }
}

impl<F, S, Req, Res> Clone for FnTransform<F, S, Req, Res>
where
    F: FnMut(Req, &mut S) -> Res + Clone,
    Res: IntoFuture,
{
    fn clone(&self) -> Self {
        FnTransform {
            f: self.f.clone(),
            _t: PhantomData,
        }
    }
}

impl<F, S, Req, Res> Transform<S> for FnTransform<F, S, Req, Res>
where
    F: FnMut(Req, &mut S) -> Res,
    Res: IntoFuture,
{
    type Request = Req;
    type Response = Res::Item;
    type Error = Res::Error;
    type Future = Res::Future;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(Async::Ready(()))
    }

    fn call(&mut self, request: Req, service: &mut S) -> Self::Future {
        (self.f)(request, service).into_future()
    }
}

impl<F, S, Req, Res> IntoTransform<FnTransform<F, S, Req, Res>, S> for F
where
    F: FnMut(Req, &mut S) -> Res,
    Res: IntoFuture,
{
    fn into_transform(self) -> FnTransform<F, S, Req, Res> {
        FnTransform::new(self)
    }
}

pub struct FnNewTransform<F, S, Req, Out, Err, Cfg>
where
    F: FnMut(Req, &mut S) -> Out + Clone,
    Out: IntoFuture,
{
    f: F,
    _t: PhantomData<(S, Req, Out, Err, Cfg)>,
}

impl<F, S, Req, Res, Err, Cfg> FnNewTransform<F, S, Req, Res, Err, Cfg>
where
    F: FnMut(Req, &mut S) -> Res + Clone,
    Res: IntoFuture,
{
    pub fn new(f: F) -> Self {
        FnNewTransform { f, _t: PhantomData }
    }
}

impl<F, S, Req, Res, Err, Cfg> NewTransform<S, Cfg> for FnNewTransform<F, S, Req, Res, Err, Cfg>
where
    F: FnMut(Req, &mut S) -> Res + Clone,
    Res: IntoFuture,
{
    type Request = Req;
    type Response = Res::Item;
    type Error = Res::Error;
    type Transform = FnTransform<F, S, Req, Res>;
    type InitError = Err;
    type Future = FutureResult<Self::Transform, Self::InitError>;

    fn new_transform(&self, _: &Cfg) -> Self::Future {
        ok(FnTransform::new(self.f.clone()))
    }
}

impl<F, S, Req, Res, Err, Cfg>
    IntoNewTransform<FnNewTransform<F, S, Req, Res, Err, Cfg>, S, Cfg> for F
where
    F: FnMut(Req, &mut S) -> Res + Clone,
    Res: IntoFuture,
{
    fn into_new_transform(self) -> FnNewTransform<F, S, Req, Res, Err, Cfg> {
        FnNewTransform::new(self)
    }
}

impl<F, S, Req, Res, Err, Cfg> Clone for FnNewTransform<F, S, Req, Res, Err, Cfg>
where
    F: FnMut(Req, &mut S) -> Res + Clone,
    Res: IntoFuture,
{
    fn clone(&self) -> Self {
        Self::new(self.f.clone())
    }
}
