use super::error::DefaultError;

/// Application state used by web services.
///
/// A [`web::App`](super::App) is parameterized by a type that implements this
/// trait. The state is available to handlers registered with
/// [`Route::to_with_state()`](super::Route::to_with_state), and to filters,
/// middleware, and services through their service context.
///
/// The associated `Error` type selects the application's error domain, which
/// is used by [`WebError`](super::WebError) and
/// [`WebResponseError`](super::WebResponseError). Most applications use
/// [`DefaultError`].
///
/// The trait does not require `Clone`, but serving an application does:
/// the server and [`HttpService`](crate::http::HttpService) clone the state
/// for each connection.
///
/// ```rust
/// use ntex::web;
///
/// #[derive(Clone)]
/// struct ApplicationState {
///     greeting: String,
/// }
///
/// impl web::State for ApplicationState {
///     type Error = web::DefaultError;
/// }
/// ```
pub trait State: 'static {
    /// Error domain used by the application.
    type Error;
}

impl State for () {
    type Error = DefaultError;
}

/// Wraps a value so it can be used as application state with [`DefaultError`].
///
/// `AppState<T>` implements [`State`] for any `'static` `T`, so a separate
/// `State` implementation is not needed. It dereferences to the wrapped value
/// and implements `Clone` and `Default` when `T` does.
///
/// ```rust
/// use ntex::web;
///
/// #[derive(Clone)]
/// struct Settings {
///     service_name: &'static str,
/// }
///
/// let state = web::AppState::new(Settings { service_name: "users" });
/// assert_eq!(state.service_name, "users");
/// ```
#[derive(Clone, Default)]
pub struct AppState<T> {
    state: T,
}

impl<T> AppState<T> {
    /// Creates application state that wraps `state`.
    pub fn new(state: T) -> Self {
        AppState { state }
    }

    /// Returns a reference to the wrapped value.
    pub fn st(&self) -> &T {
        &self.state
    }
}

impl<T: 'static> State for AppState<T> {
    type Error = DefaultError;
}

impl<T> std::ops::Deref for AppState<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.state
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, convert::Infallible, rc::Rc};

    use super::*;
    use crate::http::{Method, StatusCode};
    use crate::service::ServiceFactory;
    use crate::web::test::{TestRequest, init_service_st, read_body};
    use crate::web::{self, App, HttpRequest, WebRequest, types::Path};

    #[derive(Clone, Default)]
    struct Counter {
        name: &'static str,
        hits: Rc<Cell<usize>>,
    }

    type St = AppState<Counter>;

    async fn index(st: &St, (): ()) -> String {
        st.hits.set(st.hits.get() + 1);
        format!("index:{}", st.name)
    }

    async fn item(st: &St, (): (), id: Path<u32>) -> String {
        format!("item:{}:{}", st.st().name, id.into_inner())
    }

    async fn fallback(st: &St, (): (), req: HttpRequest) -> String {
        format!("fallback:{}:{}", st.name, req.method())
    }

    async fn filtered(_: &St, n: usize) -> String {
        format!("filtered:{n}")
    }

    #[test]
    fn app_state() {
        let st = AppState::new(Counter {
            name: "test",
            ..Counter::default()
        });
        assert_eq!(st.st().name, "test");
        assert_eq!(st.name, "test");
        assert_eq!(st.clone().name, "test");
        assert_eq!(AppState::<Counter>::default().name, "");
    }

    #[crate::rt_test]
    async fn state_handlers() {
        let st = AppState::new(Counter {
            name: "app",
            ..Counter::default()
        });
        let srv = init_service_st(
            st.clone(),
            App::<St>::new()
                .service(web::resource("/index").to_with_state(index))
                .service(web::resource("/item/{id}").to_with_state(item))
                .service(
                    web::resource("/multi")
                        .route(web::get().to(async || "get"))
                        .route(web::delete().to_with_state(index))
                        .to(async || "any"),
                )
                .service(
                    web::resource("/fallback")
                        .route(web::get().to(async || "get"))
                        .to_with_state(fallback),
                )
                .service(
                    web::resource("/filtered")
                        .filter(async |req: WebRequest<()>| {
                            Ok::<_, Infallible>(req.map_state(|()| 7usize))
                        })
                        .to_with_state(filtered),
                )
                .route("/route", web::to_with_state(index))
                .build(),
        )
        .await;

        let check = async |method: Method, uri: &str, status: StatusCode, body: &str| {
            let req = TestRequest::with_uri(uri).method(method).to_request();
            let resp = srv.call(req).await.unwrap();
            assert_eq!(resp.status(), status, "{uri}");
            assert_eq!(read_body(resp).await, body.as_bytes(), "{uri}");
        };

        check(Method::GET, "/index", StatusCode::OK, "index:app").await;
        check(Method::GET, "/item/10", StatusCode::OK, "item:app:10").await;
        check(
            Method::GET,
            "/item/abc",
            StatusCode::NOT_FOUND,
            "Path deserialize error: can not parse \"abc\" to a u32",
        )
        .await;
        check(Method::GET, "/multi", StatusCode::OK, "get").await;
        check(Method::DELETE, "/multi", StatusCode::OK, "index:app").await;
        check(Method::POST, "/multi", StatusCode::OK, "any").await;
        check(Method::GET, "/fallback", StatusCode::OK, "get").await;
        check(
            Method::POST,
            "/fallback",
            StatusCode::OK,
            "fallback:app:POST",
        )
        .await;
        check(Method::GET, "/filtered", StatusCode::OK, "filtered:7").await;
        check(Method::PUT, "/route", StatusCode::OK, "index:app").await;
        assert_eq!(st.hits.get(), 3);
    }

    #[crate::rt_test]
    async fn build_with() {
        let st = AppState::new(Counter {
            name: "fixed",
            ..Counter::default()
        });
        let srv = App::<St>::new()
            .route("/", web::get().to_with_state(index))
            .build_with::<usize>(st.clone())
            .pipeline(1usize)
            .await
            .unwrap();

        let resp = srv.call(TestRequest::default().to_request()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let resp = srv
            .call(TestRequest::with_uri("/missing").to_request())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_eq!(st.hits.get(), 1);
    }
}
