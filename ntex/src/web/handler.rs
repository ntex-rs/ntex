use std::{fmt, future::Future, marker::PhantomData};

use crate::util::BoxFuture;

use super::error::ErrorRenderer;
use super::extract::FromRequest;
use super::request::WebRequest;
use super::responder::Responder;
use super::response::WebResponse;

/// Async fn handler
pub trait Handler<T, Err>
where
    Err: ErrorRenderer,
{
    type Output: Responder<Err>;
    type Future<'f>: Future<Output = Self::Output>
    where
        Self: 'f;

    fn call(&self, param: T) -> Self::Future<'_>;
}

impl<F, R, Err> Handler<(), Err> for F
where
    F: Fn() -> R,
    R: Future,
    R::Output: Responder<Err>,
    Err: ErrorRenderer,
{
    type Future<'f> = R where Self: 'f;
    type Output = R::Output;

    fn call(&self, _: ()) -> R {
        (self)()
    }
}

pub(super) trait HandlerFn<Err: ErrorRenderer>: fmt::Debug {
    fn call(
        &self,
        _: WebRequest<Err>,
    ) -> BoxFuture<'_, Result<WebResponse, Err::Container>>;
}

pub(super) struct HandlerWrapper<F, T, Err> {
    hnd: F,
    _t: PhantomData<(T, Err)>,
}

impl<F, T, Err> HandlerWrapper<F, T, Err> {
    pub(super) fn new(hnd: F) -> Self {
        HandlerWrapper {
            hnd,
            _t: PhantomData,
        }
    }
}

impl<F, T, Err> fmt::Debug for HandlerWrapper<F, T, Err> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Handler({:?})", std::any::type_name::<F>())
    }
}

impl<F, T, Err> HandlerFn<Err> for HandlerWrapper<F, T, Err>
where
    F: Handler<T, Err> + 'static,
    T: FromRequest<Err> + 'static,
    T::Error: Into<Err::Container>,
    Err: ErrorRenderer,
{
    fn call(
        &self,
        req: WebRequest<Err>,
    ) -> BoxFuture<'_, Result<WebResponse, Err::Container>> {
        Box::pin(async move {
            let (req, mut payload) = req.into_parts();
            let param = match T::from_request(&req, &mut payload).await {
                Ok(param) => param,
                Err(e) => return Ok(WebResponse::from_err::<Err, _>(e, req)),
            };

            let result = self.hnd.call(param).await;
            let response = result.respond_to(&req).await;
            Ok(WebResponse::new(response, req))
        })
    }
}

/// FromRequest trait impl for tuples
macro_rules! factory_tuple ({ $(($n:tt, $T:ident)),+} => {
    impl<Func, $($T,)+ Res, Err> Handler<($($T,)+), Err> for Func
    where Func: Fn($($T,)+) -> Res + 'static,
          Res: Future + 'static,
          Res::Output: Responder<Err>,
          Err: ErrorRenderer,
    {
        type Future<'f> = Res where Self: 'f;
        type Output = Res::Output;

        fn call(&self, param: ($($T,)+)) -> Res {
            (self)($(param.$n,)+)
        }
    }
});

#[rustfmt::skip]
mod m {
    use super::*;

factory_tuple!((0, A));
factory_tuple!((0, A), (1, B));
factory_tuple!((0, A), (1, B), (2, C));
factory_tuple!((0, A), (1, B), (2, C), (3, D));
factory_tuple!((0, A), (1, B), (2, C), (3, D), (4, E));
factory_tuple!((0, A), (1, B), (2, C), (3, D), (4, E), (5, F));
factory_tuple!((0, A), (1, B), (2, C), (3, D), (4, E), (5, F), (6, G));
factory_tuple!((0, A), (1, B), (2, C), (3, D), (4, E), (5, F), (6, G), (7, H));
factory_tuple!((0, A), (1, B), (2, C), (3, D), (4, E), (5, F), (6, G), (7, H), (8, I));
factory_tuple!((0, A), (1, B), (2, C), (3, D), (4, E), (5, F), (6, G), (7, H), (8, I), (9, J));
}
