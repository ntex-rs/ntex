//! Json extractor/responder

use std::{fmt, future::Future, ops, pin::Pin, sync::Arc, task::Context, task::Poll};

use bytes::BytesMut;
use serde::de::DeserializeOwned;
use serde::Serialize;

#[cfg(feature = "compress")]
use crate::http::encoding::Decoder;
use crate::http::header::CONTENT_LENGTH;
use crate::http::{HttpMessage, Payload, Response, StatusCode};
use crate::util::next;
use crate::web::error::{ErrorRenderer, JsonError, JsonPayloadError, WebResponseError};
use crate::web::responder::{Ready, Responder};
use crate::web::{FromRequest, HttpRequest};

/// Json helper
///
/// Json can be used for two different purpose. First is for json response
/// generation and second is for extracting typed information from request's
/// payload.
///
/// To extract typed information from request's body, the type `T` must
/// implement the `Deserialize` trait from *serde*.
///
/// [**JsonConfig**](struct.JsonConfig.html) allows to configure extraction
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
///     let app = web::App::new().service(
///        web::resource("/index.html").route(
///            web::post().to(index))
///     );
/// }
/// ```
///
/// The `Json` type allows you to respond with well-formed JSON data: simply
/// return a value of type Json<T> where T is the type of a structure
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

impl<T: Serialize, Err: ErrorRenderer> Responder<Err> for Json<T>
where
    Err::Container: From<JsonError>,
{
    type Error = JsonError;
    type Future = Ready<Response>;

    fn respond_to(self, req: &HttpRequest) -> Self::Future {
        let body = match serde_json::to_string(&self.0) {
            Ok(body) => body,
            Err(e) => return e.error_response(req).into(),
        };

        Response::build(StatusCode::OK)
            .content_type("application/json")
            .body(body)
            .into()
    }
}

/// Json extractor. Allow to extract typed information from request's
/// payload.
///
/// To extract typed information from request's body, the type `T` must
/// implement the `Deserialize` trait from *serde*.
///
/// [**JsonConfig**](struct.JsonConfig.html) allows to configure extraction
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
///     let app = web::App::new().service(
///         web::resource("/index.html").route(
///            web::post().to(index))
///     );
/// }
/// ```
impl<T, Err: ErrorRenderer> FromRequest<Err> for Json<T>
where
    T: DeserializeOwned + 'static,
{
    type Error = JsonPayloadError;
    type Future = Pin<Box<dyn Future<Output = Result<Self, Self::Error>>>>;

    #[inline]
    fn from_request(req: &HttpRequest, payload: &mut Payload) -> Self::Future {
        let req2 = req.clone();
        let (limit, ctype) = req
            .app_data::<JsonConfig>()
            .map(|c| (c.limit, c.content_type.clone()))
            .unwrap_or((32768, None));

        let fut = JsonBody::new(req, payload, ctype).limit(limit);
        Box::pin(async move {
            match fut.await {
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
        })
    }
}

/// Json extractor configuration
///
/// ```rust
/// use ntex::http::error;
/// use ntex::web::{self, App, FromRequest, HttpResponse};
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
///     let app = App::new().service(
///         web::resource("/index.html")
///             .app_data(
///                 // change json extractor configuration
///                 web::types::JsonConfig::default()
///                    .limit(4096)
///                    .content_type(|mime| {  // <- accept text/plain content type
///                        mime.type_() == mime::TEXT && mime.subtype() == mime::PLAIN
///                    })
///             )
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
    /// Change max size of payload. By default max size is 32Kb
    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    /// Set predicate for allowed content types
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

/// Request's payload json parser, it resolves to a deserialized `T` value.
///
/// Returns error:
///
/// * content type is not `application/json`
///   (unless specified in [`JsonConfig`](struct.JsonConfig.html))
/// * content length is greater than 256k
struct JsonBody<U> {
    limit: usize,
    length: Option<usize>,
    #[cfg(feature = "compress")]
    stream: Option<Decoder<Payload>>,
    #[cfg(not(feature = "compress"))]
    stream: Option<Payload>,
    err: Option<JsonPayloadError>,
    fut: Option<Pin<Box<dyn Future<Output = Result<U, JsonPayloadError>>>>>,
}

impl<U> JsonBody<U>
where
    U: DeserializeOwned + 'static,
{
    /// Create `JsonBody` for request.
    fn new(
        req: &HttpRequest,
        payload: &mut Payload,
        ctype: Option<Arc<dyn Fn(mime::Mime) -> bool + Send + Sync>>,
    ) -> Self {
        // check content-type
        let json = if let Ok(Some(mime)) = req.mime_type() {
            mime.subtype() == mime::JSON
                || mime.suffix() == Some(mime::JSON)
                || ctype.as_ref().map_or(false, |predicate| predicate(mime))
        } else {
            false
        };

        if !json {
            return JsonBody {
                limit: 262_144,
                length: None,
                stream: None,
                fut: None,
                err: Some(JsonPayloadError::ContentType),
            };
        }

        let len = req
            .headers()
            .get(&CONTENT_LENGTH)
            .and_then(|l| l.to_str().ok())
            .and_then(|s| s.parse::<usize>().ok());

        #[cfg(feature = "compress")]
        let payload = Decoder::from_headers(payload.take(), req.headers());
        #[cfg(not(feature = "compress"))]
        let payload = payload.take();

        JsonBody {
            limit: 262_144,
            length: len,
            stream: Some(payload),
            fut: None,
            err: None,
        }
    }

    /// Change max size of payload. By default max size is 256Kb
    fn limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }
}

impl<U> Future for JsonBody<U>
where
    U: DeserializeOwned + 'static,
{
    type Output = Result<U, JsonPayloadError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(ref mut fut) = self.fut {
            return Pin::new(fut).poll(cx);
        }

        if let Some(err) = self.err.take() {
            return Poll::Ready(Err(err));
        }

        let limit = self.limit;
        if let Some(len) = self.length.take() {
            if len > limit {
                return Poll::Ready(Err(JsonPayloadError::Overflow));
            }
        }
        let mut stream = self.stream.take().unwrap();

        self.fut = Some(Box::pin(async move {
            let mut body = BytesMut::with_capacity(8192);

            while let Some(item) = next(&mut stream).await {
                let chunk = item?;
                if (body.len() + chunk.len()) > limit {
                    return Err(JsonPayloadError::Overflow);
                } else {
                    body.extend_from_slice(&chunk);
                }
            }
            Ok(serde_json::from_slice::<U>(&body)?)
        }));

        self.poll(cx)
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;
    use crate::http::header;
    use crate::web::test::{from_request, respond_to, TestRequest};

    #[derive(
        serde::Serialize, serde::Deserialize, PartialEq, Debug, derive_more::Display,
    )]
    struct MyObject {
        name: String,
    }

    fn json_eq(err: JsonPayloadError, other: JsonPayloadError) -> bool {
        if let JsonPayloadError::Overflow = err {
            if let JsonPayloadError::Overflow = other {
                return true;
            }
        } else if let JsonPayloadError::ContentType = err {
            if let JsonPayloadError::ContentType = other {
                return true;
            }
        }
        false
    }

    #[test]
    fn test_json() {
        let mut j = Json(MyObject {
            name: "test2".to_string(),
        });
        assert_eq!(j.name, "test2");
        j.name = "test".to_string();
        assert_eq!(j.name, "test");
        assert!(format!("{:?}", j).contains("Json"));
        assert!(format!("{}", j).contains("test"));
    }

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

        assert_eq!(resp.body().get_ref(), b"{\"name\":\"test\"}");
    }

    #[crate::rt_test]
    async fn test_extract() {
        let (req, mut pl) = TestRequest::default()
            .header(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("application/json"),
            )
            .header(
                header::CONTENT_LENGTH,
                header::HeaderValue::from_static("16"),
            )
            .set_payload(Bytes::from_static(b"{\"name\": \"test\"}"))
            .to_http_parts();

        let s = from_request::<Json<MyObject>>(&req, &mut pl).await.unwrap();
        assert_eq!(s.name, "test");
        assert_eq!(
            s.into_inner(),
            MyObject {
                name: "test".to_string()
            }
        );

        let (req, mut pl) = TestRequest::default()
            .header(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("application/json"),
            )
            .header(
                header::CONTENT_LENGTH,
                header::HeaderValue::from_static("16"),
            )
            .set_payload(Bytes::from_static(b"{\"name\": \"test\"}"))
            .data(JsonConfig::default().limit(10))
            .to_http_parts();

        let s = from_request::<Json<MyObject>>(&req, &mut pl).await;
        assert!(format!("{}", s.err().unwrap())
            .contains("Json payload size is bigger than allowed"));
    }

    #[crate::rt_test]
    async fn test_json_body() {
        let (req, mut pl) = TestRequest::default().to_http_parts();
        let json = JsonBody::<MyObject>::new(&req, &mut pl, None).await;
        assert!(json_eq(json.err().unwrap(), JsonPayloadError::ContentType));

        let (req, mut pl) = TestRequest::default()
            .header(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("application/text"),
            )
            .to_http_parts();
        let json = JsonBody::<MyObject>::new(&req, &mut pl, None).await;
        assert!(json_eq(json.err().unwrap(), JsonPayloadError::ContentType));

        let (req, mut pl) = TestRequest::default()
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
        assert!(json_eq(json.err().unwrap(), JsonPayloadError::Overflow));

        let (req, mut pl) = TestRequest::default()
            .header(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("application/json"),
            )
            .header(
                header::CONTENT_LENGTH,
                header::HeaderValue::from_static("16"),
            )
            .set_payload(Bytes::from_static(b"{\"name\": \"test\"}"))
            .to_http_parts();

        let json = JsonBody::<MyObject>::new(&req, &mut pl, None).await;
        assert_eq!(
            json.ok().unwrap(),
            MyObject {
                name: "test".to_owned()
            }
        );
    }

    #[crate::rt_test]
    async fn test_with_json_and_bad_content_type() {
        let (req, mut pl) = TestRequest::with_header(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("text/plain"),
        )
        .header(
            header::CONTENT_LENGTH,
            header::HeaderValue::from_static("16"),
        )
        .set_payload(Bytes::from_static(b"{\"name\": \"test\"}"))
        .data(JsonConfig::default().limit(4096))
        .to_http_parts();

        let s = from_request::<Json<MyObject>>(&req, &mut pl).await;
        assert!(s.is_err())
    }

    #[crate::rt_test]
    async fn test_with_json_and_good_custom_content_type() {
        let (req, mut pl) = TestRequest::with_header(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("text/plain"),
        )
        .header(
            header::CONTENT_LENGTH,
            header::HeaderValue::from_static("16"),
        )
        .set_payload(Bytes::from_static(b"{\"name\": \"test\"}"))
        .data(JsonConfig::default().content_type(|mime: mime::Mime| {
            mime.type_() == mime::TEXT && mime.subtype() == mime::PLAIN
        }))
        .to_http_parts();

        let s = from_request::<Json<MyObject>>(&req, &mut pl).await;
        assert!(s.is_ok())
    }

    #[crate::rt_test]
    async fn test_with_json_and_bad_custom_content_type() {
        let (req, mut pl) = TestRequest::with_header(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("text/html"),
        )
        .header(
            header::CONTENT_LENGTH,
            header::HeaderValue::from_static("16"),
        )
        .set_payload(Bytes::from_static(b"{\"name\": \"test\"}"))
        .data(JsonConfig::default().content_type(|mime: mime::Mime| {
            mime.type_() == mime::TEXT && mime.subtype() == mime::PLAIN
        }))
        .to_http_parts();

        let s = from_request::<Json<MyObject>>(&req, &mut pl).await;
        assert!(s.is_err())
    }
}
