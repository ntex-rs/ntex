use std::{fmt, future::Future, marker::PhantomData};

use super::{AppState, FromRequest, Responder, WebRequest, WebResponse, WebResponseError};
use crate::util::BoxFuture;

/// Async fn handler
pub trait Handler<St, T>: 'static
where
    St: AppState,
{
    type Output: Responder<St>;

    fn call(&self, param: T) -> impl Future<Output = Self::Output>;
}

impl<St, F, R> Handler<St, ()> for F
where
    F: AsyncFn() -> R + 'static,
    R: Responder<St>,
    St: AppState,
{
    type Output = R;

    #[allow(clippy::ignored_unit_patterns)]
    async fn call(&self, _: ()) -> R {
        (self)().await
    }
}

pub(super) trait HandlerFn<St: AppState, U>: fmt::Debug {
    fn call<'a>(&'a self, _: &'a St, _: WebRequest<U>) -> BoxFuture<'a, WebResponse>;
}

pub(super) struct HandlerWrapper<St, U, F, T> {
    hnd: F,
    _t: PhantomData<(St, U, T)>,
}

impl<St, U, F, T> HandlerWrapper<St, U, F, T> {
    pub(super) fn new(hnd: F) -> Self {
        HandlerWrapper {
            hnd,
            _t: PhantomData,
        }
    }
}

impl<St, U, F, T> fmt::Debug for HandlerWrapper<St, U, F, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Handler({:?})", std::any::type_name::<F>())
    }
}

impl<St, U, F, T> HandlerFn<St, U> for HandlerWrapper<St, U, F, T>
where
    F: Handler<St, T> + 'static,
    T: FromRequest<St> + 'static,
    T::Error: WebResponseError<St, St::Error>,
    St: AppState,
{
    fn call<'a>(&'a self, st: &'a St, req: WebRequest<U>) -> BoxFuture<'a, WebResponse> {
        Box::pin(async move {
            let (req, mut payload, _reqst) = req.into_parts();
            let param = match T::from_request(st, &req, &mut payload).await {
                Ok(param) => param,
                Err(e) => return WebResponse::from_err(st, e, req),
            };

            let result = self.hnd.call(param).await;
            let response = result.respond_to(st, &req).await;
            WebResponse::new(response, req)
        })
    }
}

/// `FromRequest` trait impl for tuples
macro_rules! factory_tuple (
    {$(#[$meta:meta])* $(($T:ident, $t:ident)),+} => {
        $(#[$meta])*
        impl<St, Func, $($T,)+ Res> Handler<St, ($($T,)+)> for Func
        where
            St: AppState,
            Func: 'static,
            Func: AsyncFn($($T,)+) -> Res,
            Res: Responder<St>,
        {
            type Output = Res;

            async fn call(&self, ($($t,)+): ($($T,)+)) -> Self::Output {
                (self)($($t,)+).await
            }
        }
    }
);

#[allow(clippy::wildcard_imports)]
#[rustfmt::skip]
mod m {
    use super::*;
    use variadics_please::all_tuples;

    // Can't use #[doc(fake_variadic)] here
    all_tuples!(factory_tuple, 1, 16, T, t);
}
