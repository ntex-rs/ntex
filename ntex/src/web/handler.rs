use std::{fmt, marker::PhantomData, rc::Rc};

use super::{FromRequest, Responder, State, WebRequest, WebResponse, WebResponseError};
use crate::util::BoxFuture;

/// Async fn handler that receives the application and request state.
///
/// Implemented for async functions and closures whose first two arguments are
/// the application state `&St` and the request state `U`, followed by up to 16
/// extractors. `T` is the tuple of extractor types. Register such handlers with
/// [`Route::to_with_state()`](crate::web::Route::to_with_state).
pub trait HandlerSt<St, U, T>: 'static
where
    St: State,
{
    /// Handler result, converted into a response.
    type Output: Responder<St>;

    /// Call the handler with the states and extracted values.
    async fn call(&self, st: &St, req: U, param: T) -> Self::Output;
}

impl<St, U, F, R> HandlerSt<St, U, ()> for F
where
    F: AsyncFn(&St, U) -> R + 'static,
    R: Responder<St>,
    St: State,
{
    type Output = R;

    #[allow(clippy::ignored_unit_patterns)]
    async fn call(&self, st: &St, req: U, _: ()) -> R {
        (self)(st, req).await
    }
}

/// Async fn handler.
///
/// Implemented for async functions and closures that take up to 16
/// extractors. `T` is the tuple of extractor types. Register such handlers with
/// [`Route::to()`](crate::web::Route::to). Use [`HandlerSt`] when the handler
/// needs the application or request state.
pub trait Handler<St, T>: 'static
where
    St: State,
{
    /// Handler result, converted into a response.
    type Output: Responder<St>;

    /// Call the handler with the extracted values.
    async fn call(&self, param: T) -> Self::Output;
}

impl<St, F, R> Handler<St, ()> for F
where
    F: AsyncFn() -> R + 'static,
    R: Responder<St>,
    St: State,
{
    type Output = R;

    #[allow(clippy::ignored_unit_patterns)]
    async fn call(&self, _: ()) -> R {
        (self)().await
    }
}

pub(super) trait HandlerFn<St: State, U>: fmt::Debug {
    fn call<'a>(&'a self, _: &'a St, _: WebRequest<U>) -> BoxFuture<'a, WebResponse>;
}

pub(super) struct HandlerStWrapper<St, U, F, T> {
    hnd: F,
    _t: PhantomData<(St, U, T)>,
}

impl<St, U, F, T> HandlerStWrapper<St, U, F, T>
where
    F: HandlerSt<St, U, T> + 'static,
    T: FromRequest<St> + 'static,
    T::Error: WebResponseError<St, St::Error>,
    St: State,
    U: 'static,
{
    pub(super) fn create(hnd: F) -> Rc<dyn HandlerFn<St, U>> {
        Rc::new(HandlerStWrapper {
            hnd,
            _t: PhantomData,
        })
    }
}

impl<St, U, F, T> fmt::Debug for HandlerStWrapper<St, U, F, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HandlerSt({:?})", std::any::type_name::<F>())
    }
}

impl<St, U, F, T> HandlerFn<St, U> for HandlerStWrapper<St, U, F, T>
where
    F: HandlerSt<St, U, T> + 'static,
    T: FromRequest<St> + 'static,
    T::Error: WebResponseError<St, St::Error>,
    St: State,
{
    fn call<'a>(&'a self, st: &'a St, req: WebRequest<U>) -> BoxFuture<'a, WebResponse> {
        Box::pin(async move {
            let (req, mut payload, reqst) = req.into_parts();
            let param = match T::from_request(st, &req, &mut payload).await {
                Ok(param) => param,
                Err(e) => return WebResponse::from_err(st, &e, req),
            };

            let result = self.hnd.call(st, reqst, param).await;
            let response = result.respond_to(st, &req).await;
            WebResponse::new(response, req)
        })
    }
}

pub(super) struct HandlerWrapper<St, U, F, T> {
    hnd: F,
    _t: PhantomData<(St, U, T)>,
}

impl<St, U, F, T> HandlerWrapper<St, U, F, T>
where
    F: Handler<St, T> + 'static,
    T: FromRequest<St> + 'static,
    T::Error: WebResponseError<St, St::Error>,
    St: State,
    U: 'static,
{
    pub(super) fn create(hnd: F) -> Rc<dyn HandlerFn<St, U>> {
        Rc::new(HandlerWrapper {
            hnd,
            _t: PhantomData,
        })
    }
}

impl<St, U, F, T> fmt::Debug for HandlerWrapper<St, U, F, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HandlerNoState({:?})", std::any::type_name::<F>())
    }
}

impl<St, U, F, T> HandlerFn<St, U> for HandlerWrapper<St, U, F, T>
where
    F: Handler<St, T> + 'static,
    T: FromRequest<St> + 'static,
    T::Error: WebResponseError<St, St::Error>,
    St: State,
{
    fn call<'a>(&'a self, st: &'a St, req: WebRequest<U>) -> BoxFuture<'a, WebResponse> {
        Box::pin(async move {
            let (req, mut payload, _) = req.into_parts();
            let param = match T::from_request(st, &req, &mut payload).await {
                Ok(param) => param,
                Err(e) => return WebResponse::from_err(st, &e, req),
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
        impl<St, Func, U, $($T,)+ Res> HandlerSt<St, U, ($($T,)+)> for Func
        where
            St: State,
            Func: 'static,
            Func: AsyncFn(&St, U, $($T,)+) -> Res,
            Res: Responder<St>,
        {
            type Output = Res;

            async fn call(&self, st: &St, req: U, ($($t,)+): ($($T,)+)) -> Self::Output {
                (self)(st, req, $($t,)+).await
            }
        }
    }
);

macro_rules! factory_tuple_no_state (
    {$(#[$meta:meta])* $(($T:ident, $t:ident)),+} => {
        $(#[$meta])*
        impl<St, Func, $($T,)+ Res> Handler<St, ($($T,)+)> for Func
        where
            St: State,
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

    all_tuples!(factory_tuple_no_state, 1, 16, T, t);
}
