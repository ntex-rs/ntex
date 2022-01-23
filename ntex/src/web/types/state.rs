use std::{ops::Deref, sync::Arc};

use crate::http::Payload;
use crate::util::{Extensions, Ready};
use crate::web::error::{DataExtractorError, ErrorRenderer};
use crate::web::extract::FromRequest;
use crate::web::httprequest::HttpRequest;

/// Application data factory
pub(crate) trait StateFactory {
    fn create(&self, extensions: &mut Extensions) -> bool;
}

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
/// state does not need to be `Send` or `Sync`. Internally `State` type
/// uses `Arc`. if your state implements `Send` + `Sync` traits you can
/// use `web::types::State::new()` and avoid double `Arc`.
///
/// If state is not set for a handler, using `State<T>` extractor would
/// cause *Internal Server Error* response.
///
/// ```rust
/// use std::sync::Mutex;
/// use ntex::web::{self, App, HttpResponse};
///
/// struct MyState {
///     counter: usize,
/// }
///
/// /// Use `State<T>` extractor to access data in handler.
/// async fn index(st: web::types::State<Mutex<MyState>>) -> HttpResponse {
///     let mut data = st.lock().unwrap();
///     data.counter += 1;
///     HttpResponse::Ok().into()
/// }
///
/// fn main() {
///     let st = web::types::State::new(Mutex::new(MyState{ counter: 0 }));
///
///     let app = App::new()
///         // Store `MyState` in application storage.
///         .app_state(st.clone())
///         .service(
///             web::resource("/index.html").route(
///                 web::get().to(index)));
/// }
/// ```
#[derive(Debug)]
pub struct State<T>(Arc<T>);

impl<T> State<T> {
    /// Create new `State` instance.
    ///
    /// Internally `State` type uses `Arc`. if your state implements
    /// `Send` + `Sync` traits you can use `web::types::State::new()` and
    /// avoid double `Arc`.
    pub fn new(state: T) -> State<T> {
        State(Arc::new(state))
    }

    /// Get reference to inner app data.
    pub fn get_ref(&self) -> &T {
        self.0.as_ref()
    }

    /// Convert to the internal Arc<T>
    pub fn into_inner(self) -> Arc<T> {
        self.0
    }
}

impl<T> Deref for State<T> {
    type Target = Arc<T>;

    fn deref(&self) -> &Arc<T> {
        &self.0
    }
}

impl<T> Clone for State<T> {
    fn clone(&self) -> State<T> {
        State(self.0.clone())
    }
}

impl<T: 'static, E: ErrorRenderer> FromRequest<E> for State<T> {
    type Error = DataExtractorError;
    type Future = Ready<Self, Self::Error>;

    #[inline]
    fn from_request(req: &HttpRequest, _: &mut Payload) -> Self::Future {
        if let Some(st) = req.app_state::<State<T>>() {
            Ready::Ok(st.clone())
        } else {
            log::debug!(
                "Failed to construct App-level Data extractor. \
                 Request path: {:?}",
                req.path()
            );
            Ready::Err(DataExtractorError::NotConfigured)
        }
    }
}

impl<T: 'static> StateFactory for State<T> {
    fn create(&self, extensions: &mut Extensions) -> bool {
        if !extensions.contains::<State<T>>() {
            extensions.insert(State(self.0.clone()));
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::http::StatusCode;
    use crate::service::Service;
    use crate::web::test::{self, init_service, TestRequest};
    use crate::web::{self, App, HttpResponse};

    #[crate::rt_test]
    async fn test_data_extractor() {
        let srv = init_service(App::new().state("TEST".to_string()).service(
            web::resource("/").to(|data: web::types::State<String>| async move {
                assert_eq!(data.to_lowercase(), "test");
                HttpResponse::Ok()
            }),
        ))
        .await;

        let req = TestRequest::default().to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let srv = init_service(
            App::new().state(10u32).service(
                web::resource("/")
                    .to(|_: web::types::State<usize>| async { HttpResponse::Ok() }),
            ),
        )
        .await;
        let req = TestRequest::default().to_request();
        let res = srv.call(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[crate::rt_test]
    async fn test_app_data_extractor() {
        let srv = init_service(
            App::new().app_state(State::new(10usize)).service(
                web::resource("/")
                    .to(|_: web::types::State<usize>| async { HttpResponse::Ok() }),
            ),
        )
        .await;

        let req = TestRequest::default().to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let srv = init_service(
            App::new().app_state(State::new(10u32)).service(
                web::resource("/")
                    .to(|_: web::types::State<usize>| async { HttpResponse::Ok() }),
            ),
        )
        .await;
        let req = TestRequest::default().to_request();
        let res = srv.call(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[crate::rt_test]
    async fn test_route_data_extractor() {
        let srv =
            init_service(App::new().service(web::resource("/").state(10usize).route(
                web::get().to(|data: web::types::State<usize>| async move {
                    let _ = data.clone();
                    HttpResponse::Ok()
                }),
            )))
            .await;

        let req = TestRequest::default().to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // different type
        let srv = init_service(App::new().service(web::resource("/").state(10u32).route(
            web::get().to(|_: web::types::State<usize>| async { HttpResponse::Ok() }),
        )))
        .await;
        let req = TestRequest::default().to_request();
        let res = srv.call(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[crate::rt_test]
    async fn test_override_data() {
        let srv = init_service(App::new().state(1usize).service(
            web::resource("/").state(10usize).route(web::get().to(
                |data: web::types::State<usize>| async move {
                    assert_eq!(**data, 10);
                    let _ = data.clone();
                    HttpResponse::Ok()
                },
            )),
        ))
        .await;

        let req = TestRequest::default().to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[cfg(feature = "tokio")]
    #[crate::rt_test]
    async fn test_data_drop() {
        struct TestData(Arc<AtomicUsize>);

        impl TestData {
            fn new(inner: Arc<AtomicUsize>) -> Self {
                let _ = inner.fetch_add(1, Ordering::SeqCst);
                Self(inner)
            }
        }

        impl Clone for TestData {
            fn clone(&self) -> Self {
                let inner = self.0.clone();
                let _ = inner.fetch_add(1, Ordering::SeqCst);
                Self(inner)
            }
        }

        impl Drop for TestData {
            fn drop(&mut self) {
                let _ = self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }

        let num = Arc::new(AtomicUsize::new(0));
        let data = TestData::new(num.clone());
        assert_eq!(num.load(Ordering::SeqCst), 1);

        let srv = test::server(move || {
            let data = data.clone();

            App::new()
                .state(data)
                .service(web::resource("/").to(|_data: State<TestData>| async { "ok" }))
        });

        assert!(srv.get("/").send().await.unwrap().status().is_success());
        srv.stop().await;

        assert_eq!(num.load(Ordering::SeqCst), 0);
    }
}
