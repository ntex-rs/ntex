//! Json extractor/responder
use std::task::{Context, Poll, ready};
use std::{fmt, future::Future, marker::PhantomData, ops, pin::Pin, sync::Arc};

use serde::{Serialize, de::DeserializeOwned};

use crate::http::header::CONTENT_LENGTH;
use crate::http::{HttpMessage, Payload, Response, StatusCode, error::PayloadError};
use crate::web::error::{JsonError, JsonPayloadError, WebResponseError};
use crate::web::{FromRequest, HttpRequest, Responder, State};

/// Json helper
///
/// Json can be used for two different purpose. First is for json response
/// generation and second is for extracting typed information from request's
/// payload.
///
/// To extract typed information from request's body, the type `T` must
/// implement the `Deserialize` trait from *serde*.
///
/// [`JsonConfig`] allows to configure extraction
/// process.
///
/// ## Example
///
/// ```rust
/// use ntex::web;
///
/// #[derive(serde::Deserialize)]
/// struct Info {
///     username: String,
/// }
///
/// /// deserialize `Info` from request's body
/// async fn index(info: web::types::Json<Info>) -> String {
///     format!("Welcome {}!", info.username)
/// }
///
/// fn main() {
///     let app = web::App::default().service(
///        web::resource("/index.html").route(
///            web::post().to(index))
///     );
/// }
/// ```
///
/// The `Json` type allows you to respond with well-formed JSON data: simply
/// return a value of type `Json<T>` where T is the type of a structure
/// to serialize into *JSON*. The type `T` must implement the `Serialize`
/// trait from *serde*.
///
/// ```rust
/// use ntex::web;
///
/// #[derive(serde::Serialize)]
/// struct MyObj {
///     name: String,
/// }
///
/// fn index(req: web::HttpRequest) -> Result<web::types::Json<MyObj>, std::io::Error> {
///     Ok(web::types::Json(MyObj {
///         name: req.match_info().get("name").unwrap().to_string(),
///     }))
/// }
/// # fn main() {}
/// ```
pub struct Json<T>(pub T);

impl<T> Json<T> {
    /// Deconstruct to an inner value
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> ops::Deref for Json<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> ops::DerefMut for Json<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

impl<T> fmt::Debug for Json<T>
where
    T: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Json").field(&self.0).finish()
    }
}

impl<T> fmt::Display for Json<T>
where
    T: fmt::Display,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl<St, T: Serialize> Responder<St> for Json<T>
where
    St: State,
    JsonError: WebResponseError<St, St::Error>,
{
    async fn respond_to(self, st: &St, _: &HttpRequest) -> Response {
        let body = match crate::http::helpers::json_body(&self.0) {
            Ok(body) => body,
            Err(e) => return e.error_response(st),
        };

        Response::builder(StatusCode::OK)
            .content_type("application/json")
            .body(body)
    }
}

/// Json extractor. Allow to extract typed information from request's
/// payload.
///
/// To extract typed information from request's body, the type `T` must
/// implement the `Deserialize` trait from *serde*.
///
/// [`JsonConfig`] allows to configure extraction
/// process.
///
/// ## Example
///
/// ```rust
/// use ntex::web;
///
/// #[derive(serde::Deserialize)]
/// struct Info {
///     username: String,
/// }
///
/// /// deserialize `Info` from request's body
/// async fn index(info: web::types::Json<Info>) -> String {
///     format!("Welcome {}!", info.username)
/// }
///
/// fn main() {
///     let app = web::App::default().service(
///         web::resource("/index.html").route(
///            web::post().to(index))
///     );
/// }
/// ```
impl<St, T> FromRequest<St> for Json<T>
where
    St: State,
    T: DeserializeOwned + 'static,
{
    type Error = JsonPayloadError;

    async fn from_request(
        _: &St,
        req: &HttpRequest,
        payload: &mut Payload,
    ) -> Result<Self, Self::Error> {
        let req2 = req.clone();
        let (limit, ctype) = req
            .app_value::<JsonConfig>()
            .map_or((32768, None), |c| (c.limit, c.content_type.as_ref()));

        match JsonBody::new(req, payload, ctype).limit(limit).await {
            Err(e) => {
                log::debug!(
                    "Failed to deserialize Json from payload. \
                     Request path: {}",
                    req2.path()
                );
                Err(e)
            }
            Ok(data) => Ok(Json(data)),
        }
    }
}

/// Json extractor configuration
///
/// Register it with [`WebAppConfig::set_value()`](crate::web::WebAppConfig::set_value)
/// and [`App::with_config()`](crate::web::App::with_config). The extractor
/// does not read the application state, so a config stored in
/// [`AppState`](crate::web::AppState) has no effect.
///
/// ```rust
/// use ntex::http::error;
/// use ntex::web::{self, App, FromRequest, HttpResponse, WebAppConfig};
///
/// #[derive(serde::Deserialize)]
/// struct Info {
///     username: String,
/// }
///
/// /// deserialize `Info` from request's body, max payload size is 4kb
/// async fn index(info: web::types::Json<Info>) -> String {
///     format!("Welcome {}!", info.username)
/// }
///
/// fn main() {
///     let cfg = WebAppConfig::new()
///         .set_value(
///             // change json extractor configuration
///             web::types::JsonConfig::default()
///                 .limit(4096)
///                 .content_type(|mime| {  // <- accept JSON and text/plain content types
///                     mime.subtype() == mime::JSON
///                         || mime.suffix() == Some(mime::JSON)
///                         || (mime.type_() == mime::TEXT && mime.subtype() == mime::PLAIN)
///                 }));
///
///     let app = App::default()
///         .with_config(cfg)
///         .service(web::resource("/index.html")
///             .route(web::post().to(index))
///     );
/// }
/// ```
#[derive(Clone)]
pub struct JsonConfig {
    limit: usize,
    content_type: Option<Arc<dyn Fn(mime::Mime) -> bool + Send + Sync>>,
}

impl JsonConfig {
    #[must_use]
    /// Change max size of payload.
    ///
    /// By default max size is 32Kb
    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    #[must_use]
    /// Set predicate for allowed content types.
    ///
    /// By default, JSON content types (`*/json` and `*/*+json`) are accepted.
    /// A predicate replaces this check: only content types for which it
    /// returns `true` are accepted, so include the JSON types in the predicate
    /// to keep accepting them. Requests without a valid `Content-Type` header
    /// are always rejected.
    pub fn content_type<F>(mut self, predicate: F) -> Self
    where
        F: Fn(mime::Mime) -> bool + Send + Sync + 'static,
    {
        self.content_type = Some(Arc::new(predicate));
        self
    }
}

impl Default for JsonConfig {
    fn default() -> Self {
        JsonConfig {
            limit: 32768,
            content_type: None,
        }
    }
}

impl fmt::Debug for JsonConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JsonConfig")
            .field("limit", &self.limit)
            .field(
                "content_type",
                &self
                    .content_type
                    .as_ref()
                    .map(|_| "Arc<dyn Fn(mime::Mime) -> bool + Send + Sync>"),
            )
            .finish()
    }
}

/// Request's payload json parser, it resolves to a deserialized `T` value.
///
/// Returns error:
///
/// * content type is not `application/json`
///   (unless specified in [`JsonConfig`])
/// * content length is greater than the limit (256k by default; the `Json`
///   extractor sets it from [`JsonConfig`], 32k by default)
struct JsonBody<U> {
    body: Option<super::ReadBody<super::BodyStream>>,
    err: Option<JsonPayloadError>,
    _t: PhantomData<fn() -> U>,
}

impl<U> JsonBody<U>
where
    U: DeserializeOwned + 'static,
{
    /// Create `JsonBody` for request.
    fn new(
        req: &HttpRequest,
        payload: &mut Payload,
        ctype: Option<&Arc<dyn Fn(mime::Mime) -> bool + Send + Sync>>,
    ) -> Self {
        // check content-type
        let json = if let Ok(Some(mime)) = req.mime_type() {
            if let Some(predicate) = ctype {
                predicate(mime)
            } else {
                mime.subtype() == mime::JSON || mime.suffix() == Some(mime::JSON)
            }
        } else {
            false
        };

        if !json {
            return Self::err(JsonPayloadError::ContentType);
        }

        let len = match req.headers().get(&CONTENT_LENGTH).map(|l| {
            l.to_str()
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .ok_or(PayloadError::UnknownLength)
        }) {
            None => None,
            Some(Ok(len)) => Some(len),
            Some(Err(e)) => return Self::err(JsonPayloadError::Payload(e)),
        };

        JsonBody {
            body: Some(super::ReadBody::from_request(req, payload, 262_144).length(len)),
            err: None,
            _t: PhantomData,
        }
    }

    fn err(err: JsonPayloadError) -> Self {
        JsonBody {
            body: None,
            err: Some(err),
            _t: PhantomData,
        }
    }

    /// Change max size of payload. By default max size is 256Kb
    fn limit(mut self, limit: usize) -> Self {
        if let Some(body) = self.body.as_mut() {
            body.limit = limit;
        }
        self
    }
}

impl<U> Future for JsonBody<U>
where
    U: DeserializeOwned + 'static,
{
    type Output = Result<U, JsonPayloadError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(err) = self.err.take() {
            return Poll::Ready(Err(err));
        }

        let body = self.body.as_mut().expect("polled after completion");
        if let Some(len) = body.length
            && len > body.limit
        {
            return Poll::Ready(Err(JsonPayloadError::Overflow));
        }
        let body = match ready!(Pin::new(body).poll(cx)) {
            Ok(body) => body,
            Err(super::ReadBodyError::Overflow(_)) => {
                return Poll::Ready(Err(JsonPayloadError::Overflow));
            }
            Err(super::ReadBodyError::Payload(err)) => {
                return Poll::Ready(Err(JsonPayloadError::Payload(err)));
            }
        };
        Poll::Ready(serde_json::from_slice::<U>(&body).map_err(JsonPayloadError::from))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::header;
    use crate::util::Bytes;
    use crate::web::test::{TestRequest, from_request, respond_to};

    #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug, thiserror::Error)]
    #[error("MyObject({name})")]
    struct MyObject {
        name: String,
    }

    fn json_eq(err: &JsonPayloadError, other: &JsonPayloadError) -> bool {
        if let JsonPayloadError::Overflow = err
            && let JsonPayloadError::Overflow = other
        {
            return true;
        } else if let JsonPayloadError::ContentType = err
            && let JsonPayloadError::ContentType = other
        {
            return true;
        }
        false
    }

    /// STATEMENT: `Json` exposes mutable data and formatting, and custom configurations remain debuggable.
    ///
    /// HOW:
    /// - Read and change the wrapped object's name and check debug/display output.
    /// - Install a custom MIME predicate and assert debug output identifies `JsonConfig`.
    #[test]
    fn test_json() {
        let mut j = Json(MyObject {
            name: "test2".to_string(),
        });
        assert_eq!(j.name, "test2");
        j.name = "test".to_string();
        assert_eq!(j.name, "test");
        assert!(format!("{j:?}").contains("Json"));
        assert!(format!("{j}").contains("test"));

        let cfg = JsonConfig::default().content_type(|mime: mime::Mime| {
            mime.type_() == mime::TEXT && mime.subtype() == mime::PLAIN
        });
        assert!(format!("{cfg:?}").contains("JsonConfig"));
    }

    /// STATEMENT: JSON responders serialize wrapped values with the JSON content type.
    ///
    /// HOW:
    /// - Respond with a named object wrapped in `Json`.
    /// - Assert OK, `application/json`, and the exact serialized body.
    #[crate::rt_test]
    async fn test_responder() {
        let req = TestRequest::default().to_http_request();

        let j = Json(MyObject {
            name: "test".to_string(),
        });
        let resp = respond_to(j, &req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            header::HeaderValue::from_static("application/json")
        );

        assert_eq!(resp.get_body_ref(), b"{\"name\":\"test\"}");
    }

    /// STATEMENT: JSON serialization failures become internal-server-error responses.
    ///
    /// HOW:
    /// - Implement a serializer that always fails and wrap its value in `Json`.
    /// - Render it and assert `INTERNAL_SERVER_ERROR`.
    #[crate::rt_test]
    async fn test_responder_serialize_error() {
        struct Invalid;

        impl Serialize for Invalid {
            fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("invalid"))
            }
        }

        let req = TestRequest::default().to_http_request();
        let resp = respond_to(Json(Invalid), &req).await;
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    /// STATEMENT: JSON extraction returns decoded objects and reports configured size or syntax errors.
    ///
    /// HOW:
    /// - Extract a valid object and assert its field and `into_inner` value.
    /// - Set a ten-byte `JsonConfig` limit and assert the overflow error text.
    /// - Supply malformed JSON and assert its error is a deserialization failure.
    #[crate::rt_test]
    async fn test_extract() {
        let (req, mut pl, ()) = TestRequest::default()
            .header(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("application/json"),
            )
            .header(
                header::CONTENT_LENGTH,
                header::HeaderValue::from_static("16"),
            )
            .payload(Bytes::from_static(b"{\"name\": \"test\"}"))
            .to_http_parts();

        let s = from_request::<_, Json<MyObject>>(&(), &req, &mut pl)
            .await
            .unwrap();
        assert_eq!(s.name, "test");
        assert_eq!(
            s.into_inner(),
            MyObject {
                name: "test".to_string()
            }
        );

        let (req, mut pl, ()) = TestRequest::default()
            .header(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("application/json"),
            )
            .header(
                header::CONTENT_LENGTH,
                header::HeaderValue::from_static("16"),
            )
            .payload(Bytes::from_static(b"{\"name\": \"test\"}"))
            .app_value(JsonConfig::default().limit(10))
            .to_http_parts();

        let s = from_request::<_, Json<MyObject>>(&(), &req, &mut pl).await;
        assert!(
            format!("{}", s.err().unwrap()).contains("Json payload size is bigger than allowed")
        );

        let (req, mut pl, ()) = TestRequest::default()
            .header(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("application/json"),
            )
            .header(
                header::CONTENT_LENGTH,
                header::HeaderValue::from_static("16"),
            )
            .payload(Bytes::from_static(b"--name-: -test--"))
            .to_http_parts();
        let s = from_request::<_, Json<serde_json::Value>>(&(), &req, &mut pl).await;
        assert!(format!("{:?}", s.err().unwrap()).contains("Deserialize(Error("));
    }

    /// STATEMENT: `Json` reads its config from the `WebAppConfig` value store, never from application state.
    ///
    /// HOW:
    /// - Put a 10-byte `JsonConfig` in `AppState` only and assert a 16-byte body still decodes.
    /// - Store the same config through the deprecated `TestRequest::app_state()` alias and
    ///   assert the size-limit error, showing the alias writes the extractor value store.
    #[crate::rt_test]
    async fn test_config_not_read_from_state() {
        let req = || {
            TestRequest::default()
                .header(
                    header::CONTENT_TYPE,
                    header::HeaderValue::from_static("application/json"),
                )
                .header(
                    header::CONTENT_LENGTH,
                    header::HeaderValue::from_static("16"),
                )
                .payload(Bytes::from_static(b"{\"name\": \"test\"}"))
        };

        let (http_req, mut pl, st) = req()
            .state(crate::web::AppState::new(JsonConfig::default().limit(10)))
            .to_http_parts();
        let s = from_request::<_, Json<MyObject>>(&st, &http_req, &mut pl).await;
        assert_eq!(s.unwrap().name, "test");

        #[allow(deprecated)]
        let (http_req, mut pl, ()) = req()
            .app_state(JsonConfig::default().limit(10))
            .to_http_parts();
        let s = from_request::<_, Json<MyObject>>(&(), &http_req, &mut pl).await;
        assert!(
            format!("{}", s.err().unwrap()).contains("Json payload size is bigger than allowed")
        );
    }

    /// STATEMENT: `JsonBody` validates media types and lengths before decoding valid JSON.
    ///
    /// HOW:
    /// - Check absent/wrong content types and an oversized declared length return the expected variants.
    /// - Decode a valid object and assert its value.
    /// - Use a malformed length and assert a nested `PayloadError::UnknownLength`.
    #[crate::rt_test]
    async fn test_json_body() {
        let (req, mut pl, ()) = TestRequest::default().to_http_parts();
        let json = JsonBody::<MyObject>::new(&req, &mut pl, None).await;
        assert!(json_eq(
            &json.err().unwrap(),
            &JsonPayloadError::ContentType
        ));

        let (req, mut pl, ()) = TestRequest::default()
            .header(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("application/text"),
            )
            .to_http_parts();
        let json = JsonBody::<MyObject>::new(&req, &mut pl, None).await;
        assert!(json_eq(
            &json.err().unwrap(),
            &JsonPayloadError::ContentType
        ));

        let (req, mut pl, ()) = TestRequest::default()
            .header(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("application/json"),
            )
            .header(
                header::CONTENT_LENGTH,
                header::HeaderValue::from_static("10000"),
            )
            .to_http_parts();

        let json = JsonBody::<MyObject>::new(&req, &mut pl, None)
            .limit(100)
            .await;
        assert!(json_eq(&json.err().unwrap(), &JsonPayloadError::Overflow));

        let (req, mut pl, ()) = TestRequest::default()
            .header(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("application/json"),
            )
            .header(
                header::CONTENT_LENGTH,
                header::HeaderValue::from_static("16"),
            )
            .payload(Bytes::from_static(b"{\"name\": \"test\"}"))
            .to_http_parts();

        let json = JsonBody::<MyObject>::new(&req, &mut pl, None).await;
        assert_eq!(
            json.ok().unwrap(),
            MyObject {
                name: "test".to_owned()
            }
        );

        let (req, mut pl, ()) = TestRequest::default()
            .header(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("application/json"),
            )
            .header(
                header::CONTENT_LENGTH,
                header::HeaderValue::from_static("16x"),
            )
            .payload(Bytes::from_static(b"{\"name\": \"test\"}"))
            .to_http_parts();

        let json = JsonBody::<MyObject>::new(&req, &mut pl, None).await;
        assert!(matches!(
            json.err().unwrap(),
            JsonPayloadError::Payload(PayloadError::UnknownLength)
        ));
    }

    /// STATEMENT: Default JSON extraction rejects a plain-text media type even with valid JSON bytes.
    ///
    /// HOW:
    /// - Supply valid JSON as `text/plain` with a configured size limit.
    /// - Extract `Json<MyObject>` and assert failure.
    #[crate::rt_test]
    async fn test_with_json_and_bad_content_type() {
        let (req, mut pl, ()) = TestRequest::with_header(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("text/plain"),
        )
        .header(
            header::CONTENT_LENGTH,
            header::HeaderValue::from_static("16"),
        )
        .payload(Bytes::from_static(b"{\"name\": \"test\"}"))
        .app_value(JsonConfig::default().limit(4096))
        .to_http_parts();

        let s = from_request::<_, Json<MyObject>>(&(), &req, &mut pl).await;
        assert!(s.is_err());
    }

    /// STATEMENT: A custom JSON content-type predicate can accept plain-text requests.
    ///
    /// HOW:
    /// - Supply valid JSON as `text/plain` and configure a predicate accepting that MIME type.
    /// - Extract `Json<MyObject>` and assert success.
    #[crate::rt_test]
    async fn test_with_json_and_good_custom_content_type() {
        let (req, mut pl, ()) = TestRequest::with_header(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("text/plain"),
        )
        .header(
            header::CONTENT_LENGTH,
            header::HeaderValue::from_static("16"),
        )
        .payload(Bytes::from_static(b"{\"name\": \"test\"}"))
        .app_value(JsonConfig::default().content_type(|mime: mime::Mime| {
            mime.type_() == mime::TEXT && mime.subtype() == mime::PLAIN
        }))
        .to_http_parts();

        let s = from_request::<_, Json<MyObject>>(&(), &req, &mut pl).await;
        assert!(s.is_ok());
    }

    /// STATEMENT: Custom JSON content-type predicates still reject nonmatching media types.
    ///
    /// HOW:
    /// - Supply valid JSON as `text/html` while the predicate accepts only `text/plain`.
    /// - Extract `Json<MyObject>` and assert failure.
    #[crate::rt_test]
    async fn test_with_json_and_bad_custom_content_type() {
        let (req, mut pl, ()) = TestRequest::with_header(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("text/html"),
        )
        .header(
            header::CONTENT_LENGTH,
            header::HeaderValue::from_static("16"),
        )
        .payload(Bytes::from_static(b"{\"name\": \"test\"}"))
        .app_value(JsonConfig::default().content_type(|mime: mime::Mime| {
            mime.type_() == mime::TEXT && mime.subtype() == mime::PLAIN
        }))
        .to_http_parts();

        let s = from_request::<_, Json<MyObject>>(&(), &req, &mut pl).await;
        assert!(s.is_err());
    }

    /// STATEMENT: A `JsonConfig::content_type()` predicate replaces the built-in JSON media-type check.
    ///
    /// HOW:
    /// - Without a predicate, assert `application/json`, `text/json` and `application/vnd.api+json`
    ///   decode while `text/plain` fails with `JsonPayloadError::ContentType`.
    /// - With a predicate accepting only `application/vnd.api+json`, assert that type decodes while
    ///   `application/json`, `text/json` and `text/plain` fail with `ContentType`.
    #[crate::rt_test]
    async fn test_content_type_predicate_replaces_default() {
        async fn extract(
            ctype: &'static str,
            cfg: Option<JsonConfig>,
        ) -> Result<(), JsonPayloadError> {
            let mut req = TestRequest::with_header(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static(ctype),
            )
            .header(
                header::CONTENT_LENGTH,
                header::HeaderValue::from_static("16"),
            )
            .payload(Bytes::from_static(b"{\"name\": \"test\"}"));
            if let Some(cfg) = cfg {
                req = req.app_value(cfg);
            }
            let (req, mut pl, ()) = req.to_http_parts();
            from_request::<_, Json<MyObject>>(&(), &req, &mut pl)
                .await
                .map(|j| assert_eq!(j.name, "test"))
        }

        for ctype in ["application/json", "text/json", "application/vnd.api+json"] {
            assert!(extract(ctype, None).await.is_ok(), "{ctype}");
        }
        assert!(matches!(
            extract("text/plain", None).await,
            Err(JsonPayloadError::ContentType)
        ));

        let only_vnd = || {
            JsonConfig::default()
                .content_type(|mime: mime::Mime| mime.essence_str() == "application/vnd.api+json")
        };
        assert!(
            extract("application/vnd.api+json", Some(only_vnd()))
                .await
                .is_ok()
        );
        for ctype in ["application/json", "text/json", "text/plain"] {
            assert!(
                matches!(
                    extract(ctype, Some(only_vnd())).await,
                    Err(JsonPayloadError::ContentType)
                ),
                "{ctype}"
            );
        }
    }
}
