//! Form extractor

use std::{fmt, future::Future, ops, pin::Pin, task::Context, task::Poll};

use bytes::BytesMut;
use encoding_rs::{Encoding, UTF_8};
use serde::de::DeserializeOwned;
use serde::Serialize;

#[cfg(feature = "compress")]
use crate::http::encoding::Decoder;
use crate::http::header::{CONTENT_LENGTH, CONTENT_TYPE};
use crate::http::{HttpMessage, Payload, Response, StatusCode};
use crate::util::next;
use crate::web::error::{ErrorRenderer, UrlencodedError, WebResponseError};
use crate::web::responder::{Ready, Responder};
use crate::web::{FromRequest, HttpRequest};

/// Form data helper (`application/x-www-form-urlencoded`)
///
/// Can be use to extract url-encoded data from the request body,
/// or send url-encoded data as the response.
///
/// ## Extract
///
/// To extract typed information from request's body, the type `T` must
/// implement the `Deserialize` trait from *serde*.
///
/// [**FormConfig**](struct.FormConfig.html) allows to configure extraction
/// process.
///
/// ### Example
/// ```rust
/// use ntex::web;
///
/// #[derive(serde::Deserialize)]
/// struct FormData {
///     username: String,
/// }
///
/// /// Extract form data using serde.
/// /// This handler get called only if content type is *x-www-form-urlencoded*
/// /// and content of the request could be deserialized to a `FormData` struct
/// fn index(form: web::types::Form<FormData>) -> String {
///     format!("Welcome {}!", form.username)
/// }
/// # fn main() {}
/// ```
///
/// ## Respond
///
/// The `Form` type also allows you to respond with well-formed url-encoded data:
/// simply return a value of type Form<T> where T is the type to be url-encoded.
/// The type  must implement `serde::Serialize`;
///
/// ### Example
/// ```rust
/// use ntex::web;
///
/// #[derive(serde::Serialize)]
/// struct SomeForm {
///     name: String,
///     age: u8
/// }
///
/// // Will return a 200 response with header
/// // `Content-Type: application/x-www-form-urlencoded`
/// // and body "name=ntex&age=123"
/// fn index() -> web::types::Form<SomeForm> {
///     web::types::Form(SomeForm {
///         name: "ntex".into(),
///         age: 123
///     })
/// }
/// # fn main() {}
/// ```
#[derive(PartialEq, Eq, PartialOrd, Ord)]
pub struct Form<T>(pub T);

impl<T> Form<T> {
    /// Deconstruct to an inner value
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> ops::Deref for Form<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> ops::DerefMut for Form<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

impl<T, Err> FromRequest<Err> for Form<T>
where
    T: DeserializeOwned + 'static,
    Err: ErrorRenderer,
{
    type Error = UrlencodedError;
    type Future = Pin<Box<dyn Future<Output = Result<Self, Self::Error>>>>;

    #[inline]
    fn from_request(req: &HttpRequest, payload: &mut Payload) -> Self::Future {
        let limit = req
            .app_data::<FormConfig>()
            .map(|c| c.limit)
            .unwrap_or(16384);

        let fut = UrlEncoded::new(req, payload).limit(limit);
        Box::pin(async move {
            match fut.await {
                Err(e) => Err(e),
                Ok(item) => Ok(Form(item)),
            }
        })
    }
}

impl<T: fmt::Debug> fmt::Debug for Form<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Form").field(&self.0).finish()
    }
}

impl<T: fmt::Display> fmt::Display for Form<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl<T: Serialize, Err: ErrorRenderer> Responder<Err> for Form<T>
where
    Err::Container: From<serde_urlencoded::ser::Error>,
{
    type Error = serde_urlencoded::ser::Error;
    type Future = Ready<Response>;

    fn respond_to(self, req: &HttpRequest) -> Self::Future {
        let body = match serde_urlencoded::to_string(&self.0) {
            Ok(body) => body,
            Err(e) => return e.error_response(req).into(),
        };

        Response::build(StatusCode::OK)
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(body)
            .into()
    }
}

/// Form extractor configuration
///
/// ```rust
/// use ntex::web::{self, App, Error, FromRequest};
///
/// #[derive(serde::Deserialize)]
/// struct FormData {
///     username: String,
/// }
///
/// /// Extract form data using serde.
/// /// Custom configuration is used for this handler, max payload size is 4k
/// async fn index(form: web::types::Form<FormData>) -> Result<String, Error> {
///     Ok(format!("Welcome {}!", form.username))
/// }
///
/// fn main() {
///     let app = App::new().service(
///         web::resource("/index.html")
///             // change `Form` extractor configuration
///             .app_data(
///                 web::types::FormConfig::default().limit(4097)
///             )
///             .route(web::get().to(index))
///     );
/// }
/// ```
#[derive(Clone, Debug)]
pub struct FormConfig {
    limit: usize,
}

impl FormConfig {
    /// Change max size of payload. By default max size is 16Kb
    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }
}

impl Default for FormConfig {
    fn default() -> Self {
        FormConfig { limit: 16384 }
    }
}

/// Future that resolves to a parsed urlencoded values.
///
/// Parse `application/x-www-form-urlencoded` encoded request's body.
/// Return `UrlEncoded` future. Form can be deserialized to any type that
/// implements `Deserialize` trait from *serde*.
///
/// Returns error:
///
/// * content type is not `application/x-www-form-urlencoded`
/// * content-length is greater than 32k
///
struct UrlEncoded<U> {
    #[cfg(feature = "compress")]
    stream: Option<Decoder<Payload>>,
    #[cfg(not(feature = "compress"))]
    stream: Option<Payload>,
    limit: usize,
    length: Option<usize>,
    encoding: &'static Encoding,
    err: Option<UrlencodedError>,
    fut: Option<Pin<Box<dyn Future<Output = Result<U, UrlencodedError>>>>>,
}

impl<U> UrlEncoded<U> {
    /// Create a new future to URL encode a request
    fn new(req: &HttpRequest, payload: &mut Payload) -> UrlEncoded<U> {
        // check content type
        if req.content_type().to_lowercase() != "application/x-www-form-urlencoded" {
            return Self::err(UrlencodedError::ContentType);
        }
        let encoding = match req.encoding() {
            Ok(enc) => enc,
            Err(_) => return Self::err(UrlencodedError::ContentType),
        };

        let mut len = None;
        if let Some(l) = req.headers().get(&CONTENT_LENGTH) {
            if let Ok(s) = l.to_str() {
                if let Ok(l) = s.parse::<usize>() {
                    len = Some(l)
                } else {
                    return Self::err(UrlencodedError::UnknownLength);
                }
            } else {
                return Self::err(UrlencodedError::UnknownLength);
            }
        };

        #[cfg(feature = "compress")]
        let payload = Decoder::from_headers(payload.take(), req.headers());
        #[cfg(not(feature = "compress"))]
        let payload = payload.take();

        UrlEncoded {
            encoding,
            stream: Some(payload),
            limit: 32_768,
            length: len,
            fut: None,
            err: None,
        }
    }

    fn err(e: UrlencodedError) -> Self {
        UrlEncoded {
            stream: None,
            limit: 32_768,
            fut: None,
            err: Some(e),
            length: None,
            encoding: UTF_8,
        }
    }

    /// Change max size of payload. By default max size is 256Kb
    fn limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }
}

impl<U> Future for UrlEncoded<U>
where
    U: DeserializeOwned + 'static,
{
    type Output = Result<U, UrlencodedError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(ref mut fut) = self.fut {
            return Pin::new(fut).poll(cx);
        }

        if let Some(err) = self.err.take() {
            return Poll::Ready(Err(err));
        }

        // payload size
        let limit = self.limit;
        if let Some(len) = self.length.take() {
            if len > limit {
                return Poll::Ready(Err(UrlencodedError::Overflow { size: len, limit }));
            }
        }

        // future
        let encoding = self.encoding;
        let mut stream = self.stream.take().unwrap();

        self.fut = Some(Box::pin(async move {
            let mut body = BytesMut::with_capacity(8192);

            while let Some(item) = next(&mut stream).await {
                let chunk = item?;
                if (body.len() + chunk.len()) > limit {
                    return Err(UrlencodedError::Overflow {
                        size: body.len() + chunk.len(),
                        limit,
                    });
                } else {
                    body.extend_from_slice(&chunk);
                }
            }

            if encoding == UTF_8 {
                serde_urlencoded::from_bytes::<U>(&body)
                    .map_err(|_| UrlencodedError::Parse)
            } else {
                let body = encoding
                    .decode_without_bom_handling_and_without_replacement(&body)
                    .map(|s| s.into_owned())
                    .ok_or(UrlencodedError::Parse)?;
                serde_urlencoded::from_str::<U>(&body)
                    .map_err(|_| UrlencodedError::Parse)
            }
        }));
        self.poll(cx)
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use serde::{Deserialize, Serialize};

    use super::*;
    use crate::http::header::{HeaderValue, CONTENT_TYPE};
    use crate::web::test::{from_request, respond_to, TestRequest};

    #[derive(Deserialize, Serialize, Debug, PartialEq, derive_more::Display)]
    #[display(fmt = "{}", "hello")]
    struct Info {
        hello: String,
        counter: i64,
    }

    fn eq(err: UrlencodedError, other: UrlencodedError) -> bool {
        if let UrlencodedError::Overflow { .. } = err {
            if let UrlencodedError::Overflow { .. } = other {
                return true;
            }
        } else if let UrlencodedError::UnknownLength = err {
            if let UrlencodedError::UnknownLength = other {
                return true;
            }
        } else if let UrlencodedError::ContentType = err {
            if let UrlencodedError::ContentType = other {
                return true;
            }
        }
        false
    }

    #[test]
    fn test_basic() {
        let mut f = Form(Info {
            hello: "world".into(),
            counter: 123,
        });
        assert_eq!(f.hello, "world");
        f.hello = "test".to_string();
        assert_eq!(f.hello, "test");
        assert!(format!("{:?}", f).contains("Form"));
        assert!(format!("{}", f).contains("test"));
    }

    #[crate::rt_test]
    async fn test_form() {
        let (req, mut pl) =
            TestRequest::with_header(CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(CONTENT_LENGTH, "11")
                .set_payload(Bytes::from_static(b"hello=world&counter=123"))
                .to_http_parts();

        let Form(s) = from_request::<Form<Info>>(&req, &mut pl).await.unwrap();
        assert_eq!(
            s,
            Info {
                hello: "world".into(),
                counter: 123
            }
        );

        let (req, mut pl) =
            TestRequest::with_header(CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(CONTENT_LENGTH, "xx")
                .set_payload(Bytes::from_static(b"hello=world&counter=123"))
                .to_http_parts();
        let res = from_request::<Form<Info>>(&req, &mut pl).await;
        assert!(eq(res.err().unwrap(), UrlencodedError::UnknownLength));
    }

    #[crate::rt_test]
    async fn test_urlencoded_error() {
        let (req, mut pl) =
            TestRequest::with_header(CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(CONTENT_LENGTH, "xxxx")
                .to_http_parts();
        let info = UrlEncoded::<Info>::new(&req, &mut pl).await;
        assert!(eq(info.err().unwrap(), UrlencodedError::UnknownLength));

        let (req, mut pl) =
            TestRequest::with_header(CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(CONTENT_LENGTH, "1000000")
                .to_http_parts();
        let info = UrlEncoded::<Info>::new(&req, &mut pl).await;
        assert!(eq(
            info.err().unwrap(),
            UrlencodedError::Overflow { size: 0, limit: 0 }
        ));

        let (req, mut pl) = TestRequest::with_header(CONTENT_TYPE, "text/plain")
            .header(CONTENT_LENGTH, "10")
            .to_http_parts();
        let info = UrlEncoded::<Info>::new(&req, &mut pl).await;
        assert!(eq(info.err().unwrap(), UrlencodedError::ContentType));
    }

    #[crate::rt_test]
    async fn test_urlencoded() {
        let (req, mut pl) =
            TestRequest::with_header(CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(CONTENT_LENGTH, "11")
                .set_payload(Bytes::from_static(b"hello=world&counter=123"))
                .to_http_parts();

        let info = UrlEncoded::<Info>::new(&req, &mut pl).await.unwrap();
        assert_eq!(
            info,
            Info {
                hello: "world".to_owned(),
                counter: 123
            }
        );

        let (req, mut pl) = TestRequest::with_header(
            CONTENT_TYPE,
            "application/x-www-form-urlencoded; charset=utf-8",
        )
        .header(CONTENT_LENGTH, "11")
        .set_payload(Bytes::from_static(b"hello=world&counter=123"))
        .to_http_parts();

        let info = UrlEncoded::<Info>::new(&req, &mut pl).await.unwrap();
        assert_eq!(
            info,
            Info {
                hello: "world".to_owned(),
                counter: 123
            }
        );

        let (req, mut pl) = TestRequest::with_header(
            CONTENT_TYPE,
            "application/x-www-form-urlencoded; charset=cp1251",
        )
        .header(CONTENT_LENGTH, "11")
        .set_payload(Bytes::from_static(b"hello=world&counter=123"))
        .to_http_parts();

        let info = UrlEncoded::<Info>::new(&req, &mut pl).await.unwrap();
        assert_eq!(
            info,
            Info {
                hello: "world".to_owned(),
                counter: 123
            }
        );
    }

    #[crate::rt_test]
    async fn test_responder() {
        let req = TestRequest::default().to_http_request();

        let form = Form(Info {
            hello: "world".to_string(),
            counter: 123,
        });
        let resp = respond_to(form, &req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(CONTENT_TYPE).unwrap(),
            HeaderValue::from_static("application/x-www-form-urlencoded")
        );

        assert_eq!(resp.body().get_ref(), b"hello=world&counter=123");
    }
}
