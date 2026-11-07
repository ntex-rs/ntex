use std::marker::{PhantomData, PhantomPinned};
use std::task::{Context, Poll};
use std::{fmt, mem::MaybeUninit, pin::Pin, ptr, rc::Rc};

use super::{FromRequest, Responder, State, WebRequest, WebResponse, WebResponseError};

/// Async fn handler that receives the application and request state.
///
/// Implemented for async functions and closures whose first two arguments are
/// the application state `&St` and the request state `U`, followed by up to 16
/// extractors. `T` is the tuple of extractor types. Register such handlers with
/// [`Route::to_with_state()`](crate::web::Route::to_with_state).
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a valid state-aware web handler",
    label = "not a `to_with_state()` handler",
    note = "a state-aware handler is an `async fn` or async closure whose first two arguments are `&St` and the request state, followed by up to 16 extractors",
    note = "the return type must implement `Responder`; for `Result<T, E>`, `E` must implement `WebResponseError`",
    note = "use `to()` for handlers that take only extractors"
)]
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
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a valid web handler",
    label = "not a web handler",
    note = "a handler is an `async fn` or async closure taking up to 16 extractors that implement `FromRequest`",
    note = "the return type must implement `Responder`; for `Result<T, E>`, `E` must implement `WebResponseError`",
    note = "use `to_with_state()` for handlers whose first arguments are `&St` and the request state"
)]
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
    fn call<'a>(&'a self, _: &'a St, _: WebRequest<U>) -> HandlerFuture<'a>;
}

/// Size of the inline handler future storage, in words.
const SLOT_WORDS: usize = 32;

/// Type-erased handler future.
///
/// Futures that fit into the inline slot are stored without allocation,
/// larger ones are boxed.
pub(super) struct HandlerFuture<'a> {
    vtable: &'static VTable,
    slot: MaybeUninit<[usize; SLOT_WORDS]>,
    _t: PhantomData<Pin<Box<dyn Future<Output = WebResponse> + 'a>>>,
    _p: PhantomPinned,
}

struct VTable {
    poll: unsafe fn(*mut (), &mut Context<'_>) -> Poll<WebResponse>,
    drop: unsafe fn(*mut ()),
}

struct VTableOf<F>(PhantomData<F>);

impl<F: Future<Output = WebResponse>> VTableOf<F> {
    const VTABLE: VTable = VTable {
        poll: poll_raw::<F>,
        drop: drop_raw::<F>,
    };
}

unsafe fn poll_raw<F: Future<Output = WebResponse>>(
    ptr: *mut (),
    cx: &mut Context<'_>,
) -> Poll<WebResponse> {
    // SAFETY: `ptr` points to an initialized `F` that is never moved out of
    // the pinned `HandlerFuture`.
    unsafe { Pin::new_unchecked(&mut *ptr.cast::<F>()).poll(cx) }
}

unsafe fn drop_raw<F>(ptr: *mut ()) {
    // SAFETY: `ptr` points to an initialized `F` that is dropped only once.
    unsafe { ptr::drop_in_place(ptr.cast::<F>()) }
}

impl<'a> HandlerFuture<'a> {
    pub(super) fn new<F: Future<Output = WebResponse> + 'a>(fut: F) -> Self {
        if Self::fits::<F>() {
            // SAFETY: `F` fits into the slot
            unsafe { Self::new_inline(fut) }
        } else {
            // SAFETY: a box is a single pointer
            unsafe { Self::new_inline(Box::pin(fut)) }
        }
    }

    const fn fits<F>() -> bool {
        size_of::<F>() <= size_of::<[usize; SLOT_WORDS]>() && align_of::<F>() <= align_of::<usize>()
    }

    /// # Safety
    ///
    /// `F` must fit into the slot.
    unsafe fn new_inline<F: Future<Output = WebResponse> + 'a>(fut: F) -> Self {
        debug_assert!(Self::fits::<F>());

        let mut this = HandlerFuture {
            vtable: &VTableOf::<F>::VTABLE,
            slot: MaybeUninit::uninit(),
            _t: PhantomData,
            _p: PhantomPinned,
        };
        // SAFETY: the slot is large enough and aligned for `F`
        unsafe { this.slot.as_mut_ptr().cast::<F>().write(fut) };
        this
    }
}

impl Future for HandlerFuture<'_> {
    type Output = WebResponse;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<WebResponse> {
        // SAFETY: the inline future is pinned together with `self`
        unsafe {
            let this = self.get_unchecked_mut();
            (this.vtable.poll)(this.slot.as_mut_ptr().cast(), cx)
        }
    }
}

impl Drop for HandlerFuture<'_> {
    fn drop(&mut self) {
        // SAFETY: the slot holds the future described by the vtable
        unsafe { (self.vtable.drop)(self.slot.as_mut_ptr().cast()) }
    }
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
    fn call<'a>(&'a self, st: &'a St, req: WebRequest<U>) -> HandlerFuture<'a> {
        HandlerFuture::new(async move {
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
    fn call<'a>(&'a self, st: &'a St, req: WebRequest<U>) -> HandlerFuture<'a> {
        HandlerFuture::new(async move {
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

#[cfg(test)]
mod tests {
    use std::{cell::Cell, future::poll_fn, rc::Rc, task::Poll};

    use super::*;
    use crate::http::{Response, StatusCode};
    use crate::web::test::TestRequest;

    struct DropGuard(Rc<Cell<usize>>);

    impl Drop for DropGuard {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    fn fits_of<F>(_: &F) -> bool {
        HandlerFuture::fits::<F>()
    }

    async fn yield_once() {
        let mut yielded = false;
        poll_fn(|cx| {
            if yielded {
                Poll::Ready(())
            } else {
                yielded = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        })
        .await;
    }

    /// STATEMENT: Inline and boxed handler futures resolve to the handler response.
    ///
    /// HOW:
    /// - Build a small future that fits the inline slot and a large one that does not.
    /// - Await both through `HandlerFuture` after one pending poll and check the status codes.
    #[crate::rt_test]
    async fn inline_and_boxed_futures_resolve() {
        let small = async {
            yield_once().await;
            TestRequest::default().to_srv_response(Response::Ok().build())
        };
        assert!(fits_of(&small));
        let res = HandlerFuture::new(small).await;
        assert_eq!(res.status(), StatusCode::OK);

        let large = async {
            let buf = [7u8; 1024];
            yield_once().await;
            let status = if buf[1023] == 7 {
                StatusCode::CREATED
            } else {
                StatusCode::BAD_REQUEST
            };
            TestRequest::default().to_srv_response(Response::new(status))
        };
        assert!(!fits_of(&large));
        let res = HandlerFuture::new(large).await;
        assert_eq!(res.status(), StatusCode::CREATED);
    }

    /// STATEMENT: Dropping a handler future runs its destructors exactly once.
    ///
    /// HOW:
    /// - Capture a drop-counting guard in inline and boxed futures.
    /// - Drop one copy of each unpolled and another after a pending poll.
    /// - Check that the counter grows by one for every dropped future.
    #[test]
    fn dropping_runs_destructor_once() {
        fn make(cnt: &Rc<Cell<usize>>, large: bool) -> HandlerFuture<'static> {
            let guard = DropGuard(cnt.clone());
            if large {
                let fut = async move {
                    let buf = [0u8; 1024];
                    yield_once().await;
                    drop((guard, buf));
                    unreachable!()
                };
                assert!(!fits_of(&fut));
                HandlerFuture::new(fut)
            } else {
                let fut = async move {
                    yield_once().await;
                    drop(guard);
                    unreachable!()
                };
                assert!(fits_of(&fut));
                HandlerFuture::new(fut)
            }
        }

        let cnt = Rc::new(Cell::new(0));
        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);

        for (idx, large) in [false, true].into_iter().enumerate() {
            drop(make(&cnt, large));
            assert_eq!(cnt.get(), idx * 2 + 1);

            let mut fut = Box::pin(make(&cnt, large));
            assert!(fut.as_mut().poll(&mut cx).is_pending());
            assert_eq!(cnt.get(), idx * 2 + 1);
            drop(fut);
            assert_eq!(cnt.get(), idx * 2 + 2);
        }
    }
}
