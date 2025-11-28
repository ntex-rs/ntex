use std::{marker::PhantomData, ops::Deref};

use crate::http::Payload;
use crate::web::error::{ErrorRenderer, StateExtractorError};
use crate::web::extract::FromRequest;
use crate::web::httprequest::HttpRequest;
use crate::web::service::AppState;

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
/// ```rust
/// use std::sync::{Arc, Mutex};
/// use ntex::web::{self, App, HttpResponse};
///
/// struct MyState {
///     counter: usize,
/// }
///
/// /// Use `State<T>` extractor to access data in handler.
/// async fn index(st: web::types::State<Arc<Mutex<MyState>>>) -> HttpResponse {
///     let mut data = st.lock().unwrap();
///     data.counter += 1;
///     HttpResponse::Ok().into()
/// }
///
/// fn main() {
///     let st = Arc::new(Mutex::new(MyState{ counter: 0 }));
///
///     let app = App::new()
///         // Store `MyState` in application storage.
///         .state(st.clone())
///         .service(
///             web::resource("/index.html").route(
///                 web::get().to(index)));
/// }
/// ```
#[derive(Debug)]
pub struct State<T>(AppState, PhantomData<T>);

impl<T: 'static> State<T> {
    /// Get reference to inner app data.
    pub fn get_ref(&self) -> &T {
        self.0.get::<T>().expect("Unexpected state")
    }
}

impl<T: 'static> Deref for State<T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.get_ref()
    }
}

impl<T> Clone for State<T> {
    fn clone(&self) -> State<T> {
        State(self.0.clone(), PhantomData)
    }
}

impl<T: 'static, E: ErrorRenderer> FromRequest<E> for State<T> {
    type Error = StateExtractorError;

    #[inline]
    async fn from_request(req: &HttpRequest, _: &mut Payload) -> Result<Self, Self::Error> {
        if req.0.app_state.contains::<T>() {
            Ok(Self(req.0.app_state.clone(), PhantomData))
        } else {
            log::debug!(
                "Failed to construct App-level State extractor. \
                 Request path: {:?}",
                req.path()
            );
            Err(StateExtractorError::NotConfigured)
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::http::StatusCode;
    use crate::web::test::{TestRequest, init_service};
    use crate::web::{self, App, HttpResponse};

    #[crate::rt_test]
    async fn test_state_extractor() {
        let srv = init_service(
            App::new().state(10usize).service(
                web::resource("/")
                    .to(|_: web::types::State<usize>| async { HttpResponse::Ok() }),
            ),
        )
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

    #[cfg(feature = "tokio")]
    #[crate::rt_test]
    async fn test_state_drop() {
        use std::sync::{Arc, atomic::AtomicUsize, atomic::Ordering};

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

        let srv = web::test::server(async move || {
            let data = data.clone();

            App::new().state(data).service(
                web::resource("/").to(|_data: super::State<TestData>| async { "ok" }),
            )
        })
        .await;

        assert!(srv.get("/").send().await.unwrap().status().is_success());
        srv.stop().await;

        assert_eq!(num.load(Ordering::SeqCst), 0);
    }

    #[crate::rt_test]
    async fn test_route_state_extractor() {
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
    async fn test_override_state() {
        let srv = init_service(App::new().state(1usize).service(
            web::resource("/").state(10usize).route(web::get().to(
                |data: web::types::State<usize>| async move {
                    assert_eq!(*data, 10);
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
}
