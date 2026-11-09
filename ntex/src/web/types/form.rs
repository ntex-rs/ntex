//! Form extractor
use std::task::{Context, Poll, ready};
use std::{borrow::Cow, fmt, future::Future, marker::PhantomData, ops, pin::Pin, str};

use encoding_rs::{Encoding, UTF_8};
use serde::{Serialize, de::DeserializeOwned};

use crate::http::header::{CONTENT_LENGTH, CONTENT_TYPE};
use crate::http::{HttpMessage, Payload, Response, StatusCode};
use crate::web::error::{UrlencodedError, WebResponseError};
use crate::web::{FromRequest, HttpRequest, Responder, State};

/// Form data helper (`application/x-www-form-urlencoded`)
///
/// Can be used to extract url-encoded data from the request body,
/// or send url-encoded data as the response.
///
/// ## Extract
///
/// To extract typed information from request's body, the type `T` must
/// implement the `Deserialize` trait from *serde*.
///
/// [`FormConfig`] allows to configure extraction
/// process.
///
/// Field names and values are decoded with the `charset` parameter of the
/// `Content-Type` header, UTF-8 by default: with `charset=windows-1252`,
/// `caf%E9` yields `café`. Raw or percent-encoded bytes that are invalid in
/// that charset fail with `UrlencodedError::Parse` rather than being replaced.
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
/// ///
/// /// If the content type is not *x-www-form-urlencoded* or the body cannot be
/// /// deserialized to a `FormData` struct, the extractor fails with
/// /// `UrlencodedError` and the handler is not called.
/// async fn index(form: web::types::Form<FormData>) -> String {
///     format!("Welcome {}!", form.username)
/// }
/// # fn main() {}
/// ```
///
/// ## Respond
///
/// The `Form` type also allows you to respond with well-formed url-encoded data:
/// simply return a value of type `Form<T>` where T is the type to be url-encoded.
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
/// async fn index() -> web::types::Form<SomeForm> {
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

impl<St, T> FromRequest<St> for Form<T>
where
    St: State,
    T: DeserializeOwned + 'static,
{
    type Error = UrlencodedError;

    async fn from_request(
        _: &St,
        req: &HttpRequest,
        payload: &mut Payload,
    ) -> Result<Self, Self::Error> {
        let limit = req.app_value::<FormConfig>().map_or(16384, |c| c.limit);

        match UrlEncoded::new(req, payload).limit(limit).await {
            Err(e) => Err(e),
            Ok(item) => Ok(Form(item)),
        }
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

impl<St, T: Serialize> Responder<St> for Form<T>
where
    St: State,
    serde_urlencoded::ser::Error: WebResponseError<St, St::Error>,
{
    async fn respond_to(self, st: &St, _: &HttpRequest) -> Response {
        let body = match serde_urlencoded::to_string(&self.0) {
            Ok(body) => body,
            Err(e) => return e.error_response(st),
        };

        Response::builder(StatusCode::OK)
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(body)
    }
}

/// Form extractor configuration
///
/// Register it with [`WebAppConfig::set_value()`](crate::web::WebAppConfig::set_value)
/// and [`App::with_config()`](crate::web::App::with_config). The extractor
/// does not read the application state, so a config stored in
/// [`AppState`](crate::web::AppState) has no effect.
///
/// ```rust
/// use std::convert::Infallible;
/// use ntex::web::{self, App, WebError, FromRequest, WebAppConfig};
///
/// #[derive(serde::Deserialize)]
/// struct FormData {
///     username: String,
/// }
///
/// /// Extract form data using serde.
/// /// Custom configuration is used for this handler, max payload size is 4k
/// async fn index(form: web::types::Form<FormData>) -> Result<String, Infallible> {
///     Ok(format!("Welcome {}!", form.username))
/// }
///
/// fn main() {
///     let cfg = WebAppConfig::new()
///         // change `Form` extractor configuration
///         .set_value(web::types::FormConfig::default().limit(4097));
///
///     let app = App::default()
///         .with_config(cfg)
///         .service(web::resource("/index.html").route(web::get().to(index)));
/// }
/// ```
#[derive(Clone, Debug)]
pub struct FormConfig {
    limit: usize,
}

impl FormConfig {
    #[must_use]
    /// Change max size of payload.
    ///
    /// By default max size is 16Kb
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
/// * content-length is greater than the limit (32k by default; the `Form`
///   extractor sets it from [`FormConfig`], 16k by default)
///
struct UrlEncoded<U> {
    body: Option<super::ReadBody<super::BodyStream>>,
    encoding: &'static Encoding,
    err: Option<UrlencodedError>,
    _t: PhantomData<fn() -> U>,
}

impl<U> UrlEncoded<U> {
    /// Create a new future to URL encode a request
    fn new(req: &HttpRequest, payload: &mut Payload) -> UrlEncoded<U> {
        // check content type
        if !req
            .content_type()
            .eq_ignore_ascii_case("application/x-www-form-urlencoded")
        {
            return Self::err(UrlencodedError::ContentType);
        }
        let Ok(encoding) = req.encoding() else {
            return Self::err(UrlencodedError::ContentType);
        };

        let mut len = None;
        if let Some(l) = req.headers().get(&CONTENT_LENGTH) {
            if let Ok(s) = l.to_str() {
                if let Ok(l) = s.parse::<usize>() {
                    len = Some(l);
                } else {
                    return Self::err(UrlencodedError::UnknownLength);
                }
            } else {
                return Self::err(UrlencodedError::UnknownLength);
            }
        }

        UrlEncoded {
            encoding,
            body: Some(super::ReadBody::from_request(req, payload, 32_768).length(len)),
            err: None,
            _t: PhantomData,
        }
    }

    fn err(e: UrlencodedError) -> Self {
        UrlEncoded {
            body: None,
            err: Some(e),
            encoding: UTF_8,
            _t: PhantomData,
        }
    }

    /// Change max size of payload. By default max size is 32Kb
    fn limit(mut self, limit: usize) -> Self {
        if let Some(body) = self.body.as_mut() {
            body.limit = limit;
        }
        self
    }
}

impl<U> Future for UrlEncoded<U>
where
    U: DeserializeOwned + 'static,
{
    type Output = Result<U, UrlencodedError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(err) = self.err.take() {
            return Poll::Ready(Err(err));
        }

        // payload size
        let encoding = self.encoding;
        let body = self.body.as_mut().expect("polled after completion");
        let limit = body.limit;
        if let Some(len) = body.length
            && len > limit
        {
            return Poll::Ready(Err(UrlencodedError::Overflow { size: len, limit }));
        }

        let body = match ready!(Pin::new(body).poll(cx)) {
            Ok(body) => body,
            Err(super::ReadBodyError::Overflow(size)) => {
                return Poll::Ready(Err(UrlencodedError::Overflow { size, limit }));
            }
            Err(super::ReadBodyError::Payload(err)) => {
                return Poll::Ready(Err(UrlencodedError::Payload(err)));
            }
        };

        Poll::Ready(
            to_utf8_form(&body, encoding)
                .ok_or(UrlencodedError::Parse)
                .and_then(|body| {
                    serde_urlencoded::from_bytes::<U>(&body).map_err(|_| UrlencodedError::Parse)
                }),
        )
    }
}

/// Converts a form body to an equivalent UTF-8 form body.
///
/// Raw and percent-decoded bytes are decoded with `encoding`, so `caf%E9`
/// with `windows-1252` yields `café`. Returns `None` if the bytes are not
/// valid in that encoding.
fn to_utf8_form<'a>(body: &'a [u8], encoding: &'static Encoding) -> Option<Cow<'a, [u8]>> {
    let (body, encoding): (Cow<'a, [u8]>, _) = if encoding.is_ascii_compatible() {
        (Cow::Borrowed(body), encoding)
    } else {
        // delimiters and escapes are only recognizable after decoding the text
        let text = encoding.decode_without_bom_handling_and_without_replacement(body)?;
        (Cow::Owned(text.into_owned().into_bytes()), UTF_8)
    };

    if encoding == UTF_8 {
        // delimiters are ASCII, so the whole body is valid iff every field is
        str::from_utf8(&percent_decode(&body)).ok()?;
        return Some(body);
    }

    let decode = |part: &[u8]| {
        encoding
            .decode_without_bom_handling_and_without_replacement(&percent_decode(part))
            .map(Cow::into_owned)
    };
    let mut fields = Vec::new();
    for field in body.split(|b| *b == b'&').filter(|f| !f.is_empty()) {
        let (name, value) = match field.iter().position(|b| *b == b'=') {
            Some(pos) => (&field[..pos], &field[pos + 1..]),
            None => (field, &b""[..]),
        };
        fields.push((decode(name)?, decode(value)?));
    }
    serde_urlencoded::to_string(fields)
        .ok()
        .map(|body| Cow::Owned(body.into_bytes()))
}

/// Decodes `+` and `%XX` escapes, keeping malformed escapes as-is.
fn percent_decode(input: &[u8]) -> Vec<u8> {
    fn hex(b: u8) -> Option<u8> {
        char::from(b)
            .to_digit(16)
            .and_then(|d| u8::try_from(d).ok())
    }

    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        match input[i] {
            b'+' => out.push(b' '),
            b'%' if let (Some(hi), Some(lo)) = (
                input.get(i + 1).copied().and_then(hex),
                input.get(i + 2).copied().and_then(hex),
            ) =>
            {
                out.push(hi << 4 | lo);
                i += 2;
            }
            b => out.push(b),
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};

    use super::*;
    use crate::http::header::HeaderValue;
    use crate::util::Bytes;
    use crate::web::test::{TestRequest, from_request, respond_to};

    #[derive(Deserialize, Serialize, Debug, PartialEq, thiserror::Error)]
    #[error("Info({hello})")]
    struct Info {
        hello: String,
        counter: i64,
    }

    fn eq(err: &UrlencodedError, other: &UrlencodedError) -> bool {
        if let UrlencodedError::Overflow { .. } = err
            && let UrlencodedError::Overflow { .. } = other
        {
            return true;
        } else if let UrlencodedError::UnknownLength = err
            && let UrlencodedError::UnknownLength = other
        {
            return true;
        } else if let UrlencodedError::ContentType = err
            && let UrlencodedError::ContentType = other
        {
            return true;
        }
        false
    }

    /// STATEMENT: `Form` exposes mutable inner fields and delegates useful formatting.
    ///
    /// HOW:
    /// - Wrap an `Info` value, read and change its `hello` field, and assert both values.
    /// - Assert debug output identifies `Form` and display output includes the changed text.
    #[test]
    fn test_basic() {
        let mut f = Form(Info {
            hello: "world".into(),
            counter: 123,
        });
        assert_eq!(f.hello, "world");
        f.hello = "test".to_string();
        assert_eq!(f.hello, "test");
        assert!(format!("{f:?}").contains("Form"));
        assert!(format!("{f}").contains("test"));
    }

    /// STATEMENT: Form extraction deserializes fields and rejects an invalid content length.
    ///
    /// HOW:
    /// - Extract `Form<Info>` from a URL-encoded body and assert both fields.
    /// - Repeat with a nonnumeric `Content-Length` and assert `UnknownLength`.
    #[crate::rt_test]
    async fn test_form() {
        let (req, mut pl, ()) =
            TestRequest::with_header(CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(CONTENT_LENGTH, "11")
                .payload(Bytes::from_static(b"hello=world&counter=123"))
                .to_http_parts();

        let Form(s) = from_request::<_, Form<Info>>(&(), &req, &mut pl)
            .await
            .unwrap();
        assert_eq!(
            s,
            Info {
                hello: "world".into(),
                counter: 123
            }
        );

        let (req, mut pl, ()) =
            TestRequest::with_header(CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(CONTENT_LENGTH, "xx")
                .payload(Bytes::from_static(b"hello=world&counter=123"))
                .to_http_parts();
        let res = from_request::<_, Form<Info>>(&(), &req, &mut pl).await;
        assert!(eq(&res.err().unwrap(), &UrlencodedError::UnknownLength));
    }

    /// STATEMENT: URL-encoded reading rejects unknown lengths, oversized declarations, and wrong types.
    ///
    /// HOW:
    /// - Construct bodies with a malformed length, a very large length, and `text/plain`.
    /// - Assert `UnknownLength`, `Overflow`, and `ContentType` error variants, respectively.
    #[crate::rt_test]
    async fn test_urlencoded_error() {
        let (req, mut pl, ()) =
            TestRequest::with_header(CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(CONTENT_LENGTH, "xxxx")
                .to_http_parts();
        let info = UrlEncoded::<Info>::new(&req, &mut pl).await;
        assert!(eq(&info.err().unwrap(), &UrlencodedError::UnknownLength));

        let (req, mut pl, ()) =
            TestRequest::with_header(CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(CONTENT_LENGTH, "1000000")
                .to_http_parts();
        let info = UrlEncoded::<Info>::new(&req, &mut pl).await;
        assert!(eq(
            &info.err().unwrap(),
            &UrlencodedError::Overflow { size: 0, limit: 0 }
        ));

        let (req, mut pl, ()) = TestRequest::with_header(CONTENT_TYPE, "text/plain")
            .header(CONTENT_LENGTH, "10")
            .to_http_parts();
        let info = UrlEncoded::<Info>::new(&req, &mut pl).await;
        assert!(eq(&info.err().unwrap(), &UrlencodedError::ContentType));
    }

    /// STATEMENT: URL-encoded reading accepts mixed-case media types and supported charset parameters.
    ///
    /// HOW:
    /// - Deserialize the same form with ordinary and mixed-case media types and UTF-8/CP1251 charsets.
    /// - Assert the expected fields for the ordinary and charset cases and success for mixed case.
    #[crate::rt_test]
    async fn test_urlencoded() {
        let (req, mut pl, ()) =
            TestRequest::with_header(CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(CONTENT_LENGTH, "11")
                .payload(Bytes::from_static(b"hello=world&counter=123"))
                .to_http_parts();

        let info = UrlEncoded::<Info>::new(&req, &mut pl).await.unwrap();
        assert_eq!(
            info,
            Info {
                hello: "world".to_owned(),
                counter: 123
            }
        );

        let (req, mut pl, ()) =
            TestRequest::with_header(CONTENT_TYPE, "Application/X-WWW-Form-URLEncoded")
                .header(CONTENT_LENGTH, "11")
                .payload(Bytes::from_static(b"hello=world&counter=123"))
                .to_http_parts();
        assert!(UrlEncoded::<Info>::new(&req, &mut pl).await.is_ok());

        let (req, mut pl, ()) = TestRequest::with_header(
            CONTENT_TYPE,
            "application/x-www-form-urlencoded; charset=utf-8",
        )
        .header(CONTENT_LENGTH, "11")
        .payload(Bytes::from_static(b"hello=world&counter=123"))
        .to_http_parts();

        let info = UrlEncoded::<Info>::new(&req, &mut pl).await.unwrap();
        assert_eq!(
            info,
            Info {
                hello: "world".to_owned(),
                counter: 123
            }
        );

        let (req, mut pl, ()) = TestRequest::with_header(
            CONTENT_TYPE,
            "application/x-www-form-urlencoded; charset=cp1251",
        )
        .header(CONTENT_LENGTH, "11")
        .payload(Bytes::from_static(b"hello=world&counter=123"))
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

    /// STATEMENT: Form fields are decoded with the declared charset and rejected if invalid.
    ///
    /// HOW:
    /// - Deserialize bodies using raw bytes, `+`, `%XX`, escaped delimiters, malformed escapes,
    ///   empty fields and fields without `=` under UTF-8, `windows-1252`, `shift_jis` and
    ///   `utf-16le` charsets.
    /// - Assert the decoded `hello` value, or `Parse` for bytes invalid in the charset.
    /// - Assert empty fields add no map entries, and a missing charset rejects invalid UTF-8.
    #[crate::rt_test]
    async fn test_urlencoded_charset() {
        let utf16 = |s: &str| -> Vec<u8> { s.encode_utf16().flat_map(u16::to_le_bytes).collect() };
        let cases: Vec<(&str, Vec<u8>, Option<&str>)> = vec![
            (
                "windows-1252",
                b"hello=caf%E9&counter=1".to_vec(),
                Some("caf\u{e9}"),
            ),
            (
                "windows-1252",
                b"hello=caf\xe9&counter=1".to_vec(),
                Some("caf\u{e9}"),
            ),
            (
                "windows-1252",
                b"hello=a%26b%3Dc+d&&counter=1&".to_vec(),
                Some("a&b=c d"),
            ),
            (
                "windows-1252",
                b"hello=100%25%zz%4&counter=1".to_vec(),
                Some("100%%zz%4"),
            ),
            ("windows-1252", b"counter=1&hello".to_vec(), Some("")),
            ("windows-1252", b"h%65llo=x&counter=1".to_vec(), Some("x")),
            (
                "shift_jis",
                b"hello=%82%A0&counter=1".to_vec(),
                Some("\u{3042}"),
            ),
            ("shift_jis", b"hello=%82&counter=1".to_vec(), None),
            ("shift_jis", b"h%82=x&counter=1".to_vec(), None),
            (
                "utf-8",
                b"hello=caf%C3%A9+x&counter=1".to_vec(),
                Some("caf\u{e9} x"),
            ),
            ("utf-8", b"hello=caf%E9&counter=1".to_vec(), None),
            ("utf-8", b"hello=caf\xff&counter=1".to_vec(), None),
            (
                "utf-16le",
                utf16("hello=caf%C3%A9&counter=1"),
                Some("caf\u{e9}"),
            ),
            ("utf-16le", utf16("hello=caf%E9&counter=1"), None),
            ("utf-16le", b"hello=x&counter=1".to_vec(), None),
        ];
        for (charset, body, expected) in cases {
            let (req, mut pl, ()) = TestRequest::with_header(
                CONTENT_TYPE,
                format!("application/x-www-form-urlencoded; charset={charset}"),
            )
            .payload(Bytes::from(body.clone()))
            .to_http_parts();
            let res = UrlEncoded::<Info>::new(&req, &mut pl).await;
            match expected {
                Some(hello) => assert_eq!(res.unwrap().hello, hello, "{charset} {body:?}"),
                None => assert!(
                    matches!(res, Err(UrlencodedError::Parse)),
                    "{charset} {body:?}"
                ),
            }
        }

        // empty fields are skipped, not decoded as an empty name
        let (req, mut pl, ()) = TestRequest::with_header(
            CONTENT_TYPE,
            "application/x-www-form-urlencoded; charset=windows-1252",
        )
        .payload(Bytes::from_static(b"&a=1&&b=2&"))
        .to_http_parts();
        let res = UrlEncoded::<std::collections::HashMap<String, String>>::new(&req, &mut pl)
            .await
            .unwrap();
        assert_eq!(res.len(), 2, "{res:?}");

        // a missing charset means UTF-8
        let (req, mut pl, ()) =
            TestRequest::with_header(CONTENT_TYPE, "application/x-www-form-urlencoded")
                .payload(Bytes::from_static(b"hello=caf%FF&counter=1"))
                .to_http_parts();
        let res = UrlEncoded::<Info>::new(&req, &mut pl).await;
        assert!(matches!(res, Err(UrlencodedError::Parse)));
    }

    /// STATEMENT: Form responders serialize fields with the URL-encoded media type.
    ///
    /// HOW:
    /// - Respond with `Form<Info>` containing a string and counter.
    /// - Assert OK, `application/x-www-form-urlencoded`, and the exact encoded body.
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

        assert_eq!(resp.get_body_ref(), b"hello=world&counter=123");
    }

    /// STATEMENT: URL-encoded reading validates charset, header encoding, and streamed size limits.
    ///
    /// HOW:
    /// - Supply an unknown charset and a non-UTF-8 length header and assert their respective errors.
    /// - Omit content length, read a body beyond a five-byte limit, and assert `Overflow`.
    #[crate::rt_test]
    async fn test_urlencoded_errors2() {
        let (req, mut pl, ()) = TestRequest::with_header(
            CONTENT_TYPE,
            "application/x-www-form-urlencoded; charset=unknown",
        )
        .to_http_parts();
        let info = UrlEncoded::<Info>::new(&req, &mut pl).await;
        assert!(eq(&info.err().unwrap(), &UrlencodedError::ContentType));

        let (req, mut pl, ()) =
            TestRequest::with_header(CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(CONTENT_LENGTH, HeaderValue::from_bytes(b"1\xff").unwrap())
                .to_http_parts();
        let info = UrlEncoded::<Info>::new(&req, &mut pl).await;
        assert!(eq(&info.err().unwrap(), &UrlencodedError::UnknownLength));

        // no content-length, payload is larger than limit
        let (req, mut pl, ()) =
            TestRequest::with_header(CONTENT_TYPE, "application/x-www-form-urlencoded")
                .payload(Bytes::from_static(b"hello=world&counter=123"))
                .to_http_parts();
        let info = UrlEncoded::<Info>::new(&req, &mut pl).limit(5).await;
        assert!(eq(
            &info.err().unwrap(),
            &UrlencodedError::Overflow { size: 0, limit: 0 }
        ));
    }

    /// STATEMENT: An unserializable form value produces an internal-server-error response.
    ///
    /// HOW:
    /// - Respond with `Form(1)`, which cannot be encoded as form fields.
    /// - Assert status `INTERNAL_SERVER_ERROR`.
    #[crate::rt_test]
    async fn test_responder_error() {
        let req = TestRequest::default().to_http_request();
        let resp = respond_to(Form(1), &req).await;
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
