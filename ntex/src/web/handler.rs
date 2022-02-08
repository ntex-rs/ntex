use std::convert::Infallible;
use std::{future::Future, marker::PhantomData, pin::Pin};

use super::error::ErrorRenderer;
use super::extract::FromRequest;
use super::request::WebRequest;
use super::responder::Responder;
use super::response::WebResponse;

/// Async fn handler
pub trait Handler<'a, T, Err>: Clone + 'static
where
    Err: ErrorRenderer,
{
    type Output: Responder<Err>;
    type Future: Future<Output = Self::Output> + 'a;

    fn call(&self, param: T) -> Self::Future;
}

impl<'a, F, R, Err> Handler<'a, (), Err> for F
where
    F: Fn() -> R + Clone + 'static,
    R: Future + 'a,
    R::Output: Responder<Err>,
    Err: ErrorRenderer,
{
    type Future = R;
    type Output = R::Output;

    fn call(&self, _: ()) -> R {
        (self)()
    }
}

pub(super) trait HandlerFn<'a, Err: ErrorRenderer> {
    fn call(
        &self,
        _: &'a mut WebRequest<'a, Err>,
    ) -> Pin<Box<dyn Future<Output = Result<WebResponse, Infallible>> + 'a>>;
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

impl<'a, F, T, Err> HandlerFn<'a, Err> for HandlerWrapper<F, T, Err>
where
    F: Handler<'a, T, Err>,
    T: FromRequest<'a, Err> + 'a,
    Err: ErrorRenderer,
{
    fn call(
        &self,
        req: &'a mut WebRequest<'a, Err>,
    ) -> Pin<Box<dyn Future<Output = Result<WebResponse, Infallible>> + 'a>> {
        let hnd = self.hnd.clone();

        Box::pin(async move {
            let r: &'a mut WebRequest<'a, Err> = unsafe { std::mem::transmute(&mut *req) };
            let fut = r.with_params(|req, pl| T::from_request(req, pl));
            let params = if let Ok(p) = fut.await {
                p
            } else {
                panic!();
            };
            let result = hnd.call(params).await;
            let response = result.respond_to(req.http_request()).await;
            Ok(WebResponse::new(response))
        })
    }
}

/// FromRequest trait impl for tuples
macro_rules! factory_tuple ({ $(($n:tt, $T:ident)),+} => {
    impl<'a, Func, $($T,)+ Res, Err> Handler<'a, ($($T,)+), Err> for Func
    where Func: Fn($($T,)+) -> Res + Clone + 'static,
          Res: Future + 'a,
          Res::Output: Responder<Err>,
          Err: ErrorRenderer,
    {
        type Future = Res;
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
