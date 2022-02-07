use std::{
    fmt, future::Future, marker::PhantomData, pin::Pin, rc::Rc, task::Context, task::Poll,
};

use super::error::ErrorRenderer;
use super::extract::FromRequest;
use super::httprequest::HttpRequest;
use super::request::WebRequest;
use super::responder::Responder;
use super::response::WebResponse;

/// Async fn handler
pub trait Handler<T, Err>: Clone + 'static
where
    Err: ErrorRenderer,
{
    type Output: Responder<Err>;
    type Future: Future<Output = Self::Output> + 'static;

    fn call(&self, param: T) -> Self::Future;
}

impl<F, R, Err> Handler<(), Err> for F
where
    F: Fn() -> R + Clone + 'static,
    R: Future + 'static,
    R::Output: Responder<Err>,
    Err: ErrorRenderer,
{
    type Future = R;
    type Output = R::Output;

    fn call(&self, _: ()) -> R {
        (self)()
    }
}

pub(super) trait HandlerFn<Err: ErrorRenderer> {
    fn call<'b>(
        &self,
        _: &'b mut WebRequest<Err>,
    ) -> Pin<Box<dyn Future<Output = Result<WebResponse, Err::Container>> + 'b>>;

    fn clone_handler(&self) -> Box<dyn HandlerFn<Err>>;
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

impl<F, T, Err> HandlerFn<Err> for HandlerWrapper<F, T, Err>
where
    F: Handler<T, Err>,
    T: FromRequest<Err> + 'static,
    T::Error: Into<Err::Container>,
    Err: ErrorRenderer,
{
    fn call<'b>(
        &self,
        req: &'b mut WebRequest<Err>,
    ) -> Pin<Box<dyn Future<Output = Result<WebResponse, Err::Container>> + 'b>> {
        let mut pl = Rc::get_mut(&mut (req.req).0).unwrap().payload.take();
        let hnd = self.hnd.clone();

        Box::pin(async move {
            let params = if let Ok(p) = T::from_request(&req.req, &mut pl).await {
                p
            } else {
                panic!();
            };
            let result = hnd.call(params).await;
            let response = result.respond_to(&req.req).await;
            Ok(WebResponse::new(response))
        })
    }

    fn clone_handler(&self) -> Box<dyn HandlerFn<Err>> {
        Box::new(HandlerWrapper {
            hnd: self.hnd.clone(),
            _t: PhantomData,
        })
    }
}

/// FromRequest trait impl for tuples
macro_rules! factory_tuple ({ $(($n:tt, $T:ident)),+} => {
    impl<Func, $($T,)+ Res, Err> Handler<($($T,)+), Err> for Func
    where Func: Fn($($T,)+) -> Res + Clone + 'static,
          Res: Future + 'static,
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
