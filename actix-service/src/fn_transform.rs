use std::marker::PhantomData;

use futures::future::{ok, FutureResult};
use futures::IntoFuture;

use crate::apply::Apply;
use crate::{IntoTransform, Service, Transform};

/// Use function as transform service
pub fn transform_fn<F, S, In, Out, Err>(
    f: F,
) -> impl Transform<S, Request = In, Response = Out::Item, Error = Out::Error, InitError = Err>
where
    S: Service,
    F: FnMut(In, &mut S) -> Out + Clone,
    Out: IntoFuture,
    Out::Error: From<S::Error>,
{
    FnTransform::new(f)
}

pub struct FnTransform<F, S, In, Out, Err>
where
    F: FnMut(In, &mut S) -> Out + Clone,
    Out: IntoFuture,
{
    f: F,
    _t: PhantomData<(S, In, Out, Err)>,
}

impl<F, S, In, Out, Err> FnTransform<F, S, In, Out, Err>
where
    F: FnMut(In, &mut S) -> Out + Clone,
    Out: IntoFuture,
{
    pub fn new(f: F) -> Self {
        FnTransform { f, _t: PhantomData }
    }
}

impl<F, S, In, Out, Err> Transform<S> for FnTransform<F, S, In, Out, Err>
where
    S: Service,
    F: FnMut(In, &mut S) -> Out + Clone,
    Out: IntoFuture,
    Out::Error: From<S::Error>,
{
    type Request = In;
    type Response = Out::Item;
    type Error = Out::Error;
    type Transform = Apply<S, F, In, Out>;
    type InitError = Err;
    type Future = FutureResult<Self::Transform, Self::InitError>;

    fn new_transform(&self, service: S) -> Self::Future {
        ok(Apply::new(service, self.f.clone()))
    }
}

impl<F, S, In, Out, Err> IntoTransform<FnTransform<F, S, In, Out, Err>, S> for F
where
    S: Service,
    F: FnMut(In, &mut S) -> Out + Clone,
    Out: IntoFuture,
    Out::Error: From<S::Error>,
{
    fn into_transform(self) -> FnTransform<F, S, In, Out, Err> {
        FnTransform::new(self)
    }
}

impl<F, S, In, Out, Err> Clone for FnTransform<F, S, In, Out, Err>
where
    F: FnMut(In, &mut S) -> Out + Clone,
    Out: IntoFuture,
{
    fn clone(&self) -> Self {
        Self::new(self.f.clone())
    }
}
