//! Various helpers for Actix applications to use during testing.
use std::cell::RefCell;

use actix_rt::{System, SystemRunner};
use actix_service::Service;
use futures::future::{lazy, Future, IntoFuture};

thread_local! {
    static RT: RefCell<Inner> = {
        RefCell::new(Inner(Some(System::builder().build())))
    };
}

struct Inner(Option<SystemRunner>);

impl Inner {
    fn get_mut(&mut self) -> &mut SystemRunner {
        self.0.as_mut().unwrap()
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        std::mem::forget(self.0.take().unwrap())
    }
}

/// Runs the provided future, blocking the current thread until the future
/// completes.
///
/// This function can be used to synchronously block the current thread
/// until the provided `future` has resolved either successfully or with an
/// error. The result of the future is then returned from this function
/// call.
///
/// Note that this function is intended to be used only for testing purpose.
/// This function panics on nested call.
pub fn block_on<F>(f: F) -> Result<F::Item, F::Error>
where
    F: IntoFuture,
{
    RT.with(move |rt| rt.borrow_mut().get_mut().block_on(f.into_future()))
}

/// Runs the provided function, blocking the current thread until the result
/// future completes.
///
/// This function can be used to synchronously block the current thread
/// until the provided `future` has resolved either successfully or with an
/// error. The result of the future is then returned from this function
/// call.
///
/// Note that this function is intended to be used only for testing purpose.
/// This function panics on nested call.
pub fn block_fn<F, R>(f: F) -> Result<R::Item, R::Error>
where
    F: FnOnce() -> R,
    R: IntoFuture,
{
    RT.with(move |rt| rt.borrow_mut().get_mut().block_on(lazy(f)))
}

/// Spawn future to the current test runtime.
pub fn spawn<F>(fut: F)
where
    F: Future<Item = (), Error = ()> + 'static,
{
    run_on(move || {
        actix_rt::spawn(fut);
    });
}

/// Runs the provided function, with runtime enabled.
///
/// Note that this function is intended to be used only for testing purpose.
/// This function panics on nested call.
pub fn run_on<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    RT.with(move |rt| {
        rt.borrow_mut()
            .get_mut()
            .block_on(lazy(|| Ok::<_, ()>(f())))
    })
    .unwrap()
}

/// Calls service and waits for response future completion.
///
/// ```rust,ignore
/// use actix_web::{test, App, HttpResponse, http::StatusCode};
/// use actix_service::Service;
///
/// #[test]
/// fn test_response() {
///     let mut app = test::init_service(
///         App::new()
///             .service(web::resource("/test").to(|| HttpResponse::Ok()))
///     );
///
///     // Create request object
///     let req = test::TestRequest::with_uri("/test").to_request();
///
///     // Call application
///     let resp = test::call_service(&mut app, req);
///     assert_eq!(resp.status(), StatusCode::OK);
/// }
/// ```
pub fn call_service<S, R>(app: &mut S, req: R) -> S::Response
where
    S: Service<Request = R>,
    S::Error: std::fmt::Debug,
{
    block_on(run_on(move || app.call(req))).unwrap()
}
