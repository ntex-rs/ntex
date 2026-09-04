use std::{convert::Infallible, ops::Deref};

use crate::http::Payload;
use crate::web::{AppState, FromRequest, HttpRequest};

/// Application state.
///
/// Application state is an arbitrary data attached to the app.
/// Application state is available to all routes and could be added
/// during application configuration process
/// with `App::state()` method.
///
/// Application state could be accessed by using `State<T>`
/// extractor where `T` is state type.
///
/// **Note**: http server accepts an application factory rather than
/// an application instance. Http server constructs an application
/// instance for each thread, thus application data must be constructed
/// multiple times. If you want to share state between different
/// threads, a shareable object should be used, e.g. `Send + Sync`. Application
/// state does not need to be `Send` or `Sync`.
///
/// If state is not set for a handler, using `State<T>` extractor would
/// cause *Internal Server Error* response.
///
/// ```rust,ignore
/// use std::cell::Cell;
/// use ntex::web::{self, App, AppState, HttpResponse, WebError};
///
/// #[derive(Default, Clone)]
/// struct MyState {
///     counter: Cell<usize>,
/// }
///
/// impl AppState for MyState {
///     type Error = WebError;
/// }
///
/// /// Use `State<T>` extractor to access data in handler.
/// async fn index(st: web::types::State<MyState>) -> HttpResponse {
///     st.counter.set(st.counter.get() + 1);
///     HttpResponse::Ok().into()
/// }
///
/// fn main() {
///     let app = App::with::<MyState>()
///         .service(
///             web::resource("/index.html").route(
///                 web::get().to(index)));
/// }
/// ```
#[derive(Debug)]
pub struct State<St>(St);

impl<St> Deref for State<St> {
    type Target = St;

    fn deref(&self) -> &St {
        &self.0
    }
}

impl<St: AppState + Clone> FromRequest<St> for State<St> {
    type Error = Infallible;

    #[inline]
    async fn from_request(st: &St, _: &HttpRequest, _: &mut Payload) -> Result<Self, Self::Error> {
        Ok(Self(st.clone()))
    }
}

#[cfg(test)]
mod tests {
    use crate::http::StatusCode;
    use crate::web::test::{TestRequest, init_service};
    use crate::web::{self, App, HttpResponse, WebError};

    use super::*;

    #[crate::rt_test]
    async fn test_state_extractor() {
        #[allow(dead_code)]
        #[derive(Clone, Default)]
        struct MyState {
            val: usize,
        }

        impl AppState for MyState {
            type Error = WebError;
        }

        let srv = init_service(
            App::<MyState>::with()
                .service(web::resource("/").to(|_: State<MyState>| async { HttpResponse::Ok() })),
        )
        .await;

        let req = TestRequest::default().to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let srv = init_service(
            App::<MyState>::with()
                .service(web::resource("/").to(|_: State<MyState>| async { HttpResponse::Ok() })),
        )
        .await;
        let req = TestRequest::default().to_request();
        let res = srv.call(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }
}
