use std::{ops::Deref, rc::Rc};

use futures::future::{err, ok, Ready};

use crate::http::Payload;
use crate::util::Extensions;
use crate::web::error::{DataExtractorError, ErrorRenderer};
use crate::web::extract::FromRequest;
use crate::web::httprequest::HttpRequest;

use crate::web::types::data::DataFactory;

/// Application data factory
pub(crate) trait RcDataFactory {
    fn create(&self, extensions: &mut Extensions) -> bool;
}

/// Application Rc data.
///
/// Application data is an arbitrary data attached to the app.
/// Application data is available to all routes and could be added
/// during application configuration process
/// with `App::data()` method.
///
/// Application data could be accessed by using `RcData<T>`
/// extractor where `T` is data type.
///
/// **Note**: http server accepts an application factory rather than
/// an application instance. Http server constructs an application
/// instance for each thread, thus application data must be constructed
/// multiple times. If you want to share data between different
/// threads, a shareable object should be used, e.g. `Send + Sync`. Application
/// data does not need to be `Send` or `Sync`. Internally `RcData` type
/// uses `Rc`. if your data implements `Send` + `Sync` traits you can
/// use `web::types::RcData::new()` and avoid double `Rc`.
///
/// If route data is not set for a handler, using `RcData<T>` extractor would
/// cause *Internal Server Error* response.
///
/// ```rust
/// use std::sync::Mutex;
/// use ntex::web::{self, App, HttpResponse};
///
/// struct MyData {
///     counter: usize,
/// }
///
/// /// Use `RcData<T>` extractor to access data in handler.
/// async fn index(data: web::types::RcData<Mutex<MyData>>) -> HttpResponse {
///     let mut data = data.lock().unwrap();
///     data.counter += 1;
///     HttpResponse::Ok().into()
/// }
///
/// fn main() {
///     let data = web::types::RcData::new(Mutex::new(MyData{ counter: 0 }));
///
///     let app = App::new()
///         // Store `MyData` in application storage.
///         .app_data(data.clone())
///         .service(
///             web::resource("/index.html").route(
///                 web::get().to(index)));
/// }
/// ```
#[derive(Debug)]
pub struct RcData<T>(Rc<T>);

impl<T> RcData<T> {
    /// Create new `RcData` instance.
    ///
    /// Internally `RcData` type uses `Rc`. if your data implements
    /// `Send` + `Sync` traits you can use `web::types::RcData::new()` and
    /// avoid double `Rc`.
    pub fn new(state: T) -> RcData<T> {
        RcData(Rc::new(state))
    }

    /// Get reference to inner app data.
    pub fn get_ref(&self) -> &T {
        self.0.as_ref()
    }

    /// Convert to the internal Rc<T>
    pub fn into_inner(self) -> Rc<T> {
        self.0
    }
}

impl<T> Deref for RcData<T> {
    type Target = Rc<T>;

    fn deref(&self) -> &Rc<T> {
        &self.0
    }
}

impl<T> Clone for RcData<T> {
    fn clone(&self) -> RcData<T> {
        RcData(self.0.clone())
    }
}

impl<T: 'static, E: ErrorRenderer> FromRequest<E> for RcData<T> {
    type Error = DataExtractorError;
    type Future = Ready<Result<Self, Self::Error>>;

    #[inline]
    fn from_request(req: &HttpRequest, _: &mut Payload) -> Self::Future {
        if let Some(st) = req.app_data::<RcData<T>>() {
            ok(st.clone())
        } else {
            log::debug!(
                "Failed to construct App-level RcData extractor. \
                 Request path: {:?}",
                req.path()
            );
            err(DataExtractorError::NotConfigured)
        }
    }
}

impl<T: 'static> RcDataFactory for RcData<T> {
    fn create(&self, extensions: &mut Extensions) -> bool {
        if !extensions.contains::<RcData<T>>() {
            extensions.insert(RcData(self.0.clone()));
            true
        } else {
            false
        }
    }
}

impl<T: 'static> DataFactory for RcData<T> {
    fn create(&self, extensions: &mut Extensions) -> bool {
        if !extensions.contains::<RcData<T>>() {
            extensions.insert(RcData(self.0.clone()));
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::http::StatusCode;
    use crate::web::test::{init_service, TestRequest};
    use crate::web::{self, App, HttpResponse};
    use crate::Service;

    #[crate::rt_test]
    async fn test_data_extractor() {
        let srv = init_service(App::new().rc_data("TEST".to_string()).service(
            web::resource("/").to(|data: web::types::RcData<String>| async move {
                assert_eq!(data.to_lowercase(), "test");
                HttpResponse::Ok()
            }),
        ))
        .await;

        let req = TestRequest::default().to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let srv = init_service(
            App::new().rc_data(10u32).service(
                web::resource("/")
                    .to(|_: web::types::RcData<usize>| async { HttpResponse::Ok() }),
            ),
        )
        .await;
        let req = TestRequest::default().to_request();
        let res = srv.call(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
