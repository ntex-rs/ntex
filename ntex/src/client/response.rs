use std::cell::{Cell, Ref, RefMut};
use std::task::{Context, Poll, ready};
use std::{fmt, future::Future, marker::PhantomData, pin::Pin};

use serde::de::DeserializeOwned;

#[cfg(feature = "cookie")]
use coo_kie::{Cookie, ParseError as CookieParseError};

use crate::Cfg;
use crate::error::Error;
use crate::http::header::{AsName, CONTENT_LENGTH, HeaderValue};
use crate::http::{HeaderMap, HttpMessage, Payload, ResponseHead, StatusCode, Version};
use crate::http::{error::PayloadError, helpers::take_trimmed};
use crate::time::{Deadline, Millis};
use crate::util::{Bytes, BytesMut, Extensions, Stream};

use super::error::{ClientPayloadError, JsonPayloadError};
use super::{ClientConfig, ServiceResponse};

/// An HTTP client response.
pub struct ClientResponse {
    pub(crate) head: ResponseHead,
    pub(crate) payload: Cell<Option<Payload>>,
    /// Set once the stream has reported `PayloadError::AlreadyTaken`.
    taken_reported: Cell<bool>,
    pub(crate) config: Cfg<ClientConfig>,
}

impl HttpMessage for ClientResponse {
    fn message_headers(&self) -> &HeaderMap {
        &self.head.headers
    }

    fn message_extensions(&self) -> Ref<'_, Extensions> {
        self.head.extensions()
    }

    fn message_extensions_mut(&self) -> RefMut<'_, Extensions> {
        self.head.extensions_mut()
    }

    #[cfg(feature = "cookie")]
    /// Parses cookies from the response `Set-Cookie` headers.
    ///
    /// Headers that are not valid UTF-8 or cannot be parsed are skipped.
    ///
    /// # Errors
    ///
    /// This implementation does not return an error.
    fn cookies(&self) -> Result<Ref<'_, Vec<Cookie<'static>>>, CookieParseError> {
        use crate::http::header::SET_COOKIE;

        struct Cookies(Vec<Cookie<'static>>);

        if self.message_extensions().get::<Cookies>().is_none() {
            let mut cookies = Vec::new();
            for hdr in self.message_headers().get_all(&SET_COOKIE) {
                if let Ok(s) = std::str::from_utf8(hdr.as_bytes())
                    && let Ok(cookie) = Cookie::parse_encoded(s)
                {
                    cookies.push(cookie.into_owned());
                }
            }
            self.message_extensions_mut().insert(Cookies(cookies));
        }
        Ok(Ref::map(self.message_extensions(), |ext| {
            &ext.get::<Cookies>().unwrap().0
        }))
    }
}

impl ClientResponse {
    /// Creates a client response.
    #[doc(hidden)]
    pub fn new(head: ResponseHead, payload: Payload, config: Cfg<ClientConfig>) -> Self {
        ClientResponse {
            head,
            config,
            payload: Cell::new(Some(payload)),
            taken_reported: Cell::new(false),
        }
    }

    #[cfg(feature = "ws")]
    pub(crate) fn with_empty_payload(head: ResponseHead, config: Cfg<ClientConfig>) -> Self {
        ClientResponse::new(head, Payload::None, config)
    }

    #[inline]
    pub(crate) fn head(&self) -> &ResponseHead {
        &self.head
    }

    #[inline]
    pub(crate) fn head_mut(&mut self) -> &mut ResponseHead {
        &mut self.head
    }

    /// Returns the response HTTP version.
    #[inline]
    pub fn version(&self) -> Version {
        self.head().version
    }

    /// Returns the response status.
    #[inline]
    pub fn status(&self) -> StatusCode {
        self.head().status
    }

    #[inline]
    /// Returns the first header value associated with `name`.
    pub fn header<N: AsName>(&self, name: N) -> Option<&HeaderValue> {
        self.head().headers.get(name)
    }

    #[inline]
    /// Returns the response headers.
    pub fn headers(&self) -> &HeaderMap {
        &self.head().headers
    }

    #[inline]
    /// Returns mutable access to the response headers.
    pub fn headers_mut(&mut self) -> &mut HeaderMap {
        &mut self.head_mut().headers
    }

    /// Replaces the response payload.
    ///
    /// Any unread previous payload is dropped. The response can be read again
    /// even if its previous payload was already taken.
    pub fn set_payload(&self, payload: Payload) {
        self.payload.set(Some(payload));
        self.taken_reported.set(false);
    }

    #[must_use]
    /// Takes the response payload.
    ///
    /// Subsequent calls return an empty payload. After the payload is taken,
    /// [`body()`](Self::body), [`json()`](Self::json) and the response stream
    /// report [`PayloadError::AlreadyTaken`] instead of an empty body.
    pub fn take_payload(&self) -> Payload {
        if let Some(pl) = self.payload.take() {
            pl
        } else {
            Payload::None
        }
    }

    /// Returns the response extensions.
    #[inline]
    pub fn extensions(&self) -> Ref<'_, Extensions> {
        self.head().extensions()
    }

    /// Returns mutable access to the response extensions.
    #[inline]
    pub fn extensions_mut(&self) -> RefMut<'_, Extensions> {
        self.head().extensions_mut()
    }
}

impl ClientResponse {
    /// Returns a future that buffers the response body.
    ///
    /// The response payload is taken when this method is called, not when the
    /// future is first polled. Dropping the future without awaiting it
    /// discards the body. Once the payload has been taken, by this method,
    /// [`json()`](Self::json), [`take_payload()`](Self::take_payload) or the
    /// response stream, a later `body()` fails with
    /// [`PayloadError::AlreadyTaken`]. A response without a body still yields
    /// empty bytes.
    ///
    /// To keep the raw body when JSON parsing fails, buffer it once and parse
    /// the retained bytes:
    ///
    /// ```rust,no_run
    /// # async fn read(res: ntex::client::ClientResponse) -> Result<(), Box<dyn std::error::Error>> {
    /// let body = res.body().await?;
    /// match serde_json::from_slice::<serde_json::Value>(&body) {
    ///     Ok(value) => println!("json: {value}"),
    ///     Err(_) => println!("raw: {} bytes", body.len()),
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn body(&self) -> MessageBody {
        MessageBody::new(self)
    }

    /// Returns a future that buffers and deserializes a JSON response body.
    ///
    /// The future returns an error when:
    ///
    /// * the content type is not JSON;
    /// * the body exceeds the configured response payload limit; or
    /// * reading or deserializing the body fails.
    ///
    /// A JSON content type is checked before the payload is taken, so a
    /// content-type error leaves the body available. Otherwise the payload is
    /// taken when this method is called, as with [`body()`](Self::body): a
    /// dropped future or a deserialization failure leaves no body, and later
    /// reads fail with [`PayloadError::AlreadyTaken`]. Use `body()` and
    /// `serde_json::from_slice()` to keep the raw bytes.
    pub fn json<T: DeserializeOwned>(&self) -> JsonBody<T> {
        JsonBody::new(self)
    }
}

impl Stream for ClientResponse {
    type Item = Result<Bytes, Error<ClientPayloadError>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Some(mut pl) = self.payload.take() {
            let result = Pin::new(&mut pl).poll_next(cx);
            self.payload.set(Some(pl));
            Poll::Ready(
                ready!(result).map(|item| item.map_err(|e| Error::from(ClientPayloadError(e)))),
            )
        } else if self.taken_reported.replace(true) {
            Poll::Ready(None)
        } else {
            Poll::Ready(Some(Err(Error::from(ClientPayloadError(
                PayloadError::AlreadyTaken,
            )))))
        }
    }
}

impl From<ServiceResponse> for ClientResponse {
    fn from(res: ServiceResponse) -> Self {
        Self {
            head: res.head,
            payload: Cell::new(Some(res.payload)),
            taken_reported: Cell::new(false),
            config: res.config,
        }
    }
}

impl fmt::Debug for ClientResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "\nClientResponse {:?} {}", self.version(), self.status())?;
        writeln!(f, "  headers:")?;
        for (key, val) in self.headers() {
            writeln!(f, "    {key:?}: {val:?}")?;
        }
        Ok(())
    }
}

#[derive(Debug)]
/// Future that buffers a complete HTTP response body.
pub struct MessageBody {
    length: Option<usize>,
    err: Option<Error<ClientPayloadError>>,
    fut: Option<ReadBody>,
    config: Cfg<ClientConfig>,
}

impl MessageBody {
    /// Creates a body future for `res`.
    ///
    /// This takes the response payload. If the payload was already taken, the
    /// future fails with [`PayloadError::AlreadyTaken`].
    pub fn new(res: &ClientResponse) -> MessageBody {
        let config = res.config.clone();

        let len = match content_length(res) {
            Ok(len) => len,
            Err(e) => return Self::err(Error::from(e).with_service(config.service()), config),
        };

        let Some(payload) = res.payload.take() else {
            return Self::err(
                Error::from(ClientPayloadError(PayloadError::AlreadyTaken))
                    .with_service(config.service()),
                config,
            );
        };

        MessageBody {
            config,
            length: len,
            err: None,
            fut: Some(ReadBody::new(
                payload,
                res.config.response_payload_limit(),
                res.config.response_payload_timeout(),
                res.config.clone(),
            )),
        }
    }

    #[must_use]
    /// Sets the maximum buffered payload size.
    ///
    /// The default is 256 KiB. A value of zero disables the limit.
    pub fn limit(mut self, limit: usize) -> Self {
        if let Some(ref mut fut) = self.fut {
            fut.limit = limit;
        }
        self
    }

    #[must_use]
    /// Sets the timeout for reading the complete payload.
    ///
    /// The default is 10 seconds. A zero duration disables the timeout.
    pub fn timeout<T: Into<Millis>>(mut self, to: T) -> Self {
        if let Some(ref mut fut) = self.fut {
            fut.timeout.reset(to.into());
        }
        self
    }

    fn err(e: Error<ClientPayloadError>, config: Cfg<ClientConfig>) -> Self {
        MessageBody {
            config,
            fut: None,
            err: Some(e),
            length: None,
        }
    }
}

impl Future for MessageBody {
    type Output = Result<Bytes, Error<ClientPayloadError>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        if let Some(err) = this.err.take() {
            return Poll::Ready(Err(err));
        }

        if let Some(len) = this.length.take() {
            let limit = this.fut.as_ref().unwrap().limit;
            if limit > 0 && len > limit {
                return Poll::Ready(Err(Error::from(ClientPayloadError(PayloadError::Overflow))
                    .with_service(this.config.service())));
            }
        }

        Pin::new(&mut *this.fut.as_mut().unwrap())
            .poll(cx)
            .map_err(|e| e.map(ClientPayloadError::from))
    }
}

#[derive(Debug)]
/// Future that buffers and deserializes a JSON response body.
///
/// The future returns an error when:
///
/// * the content type is not JSON;
/// * the body exceeds the configured response payload limit; or
/// * reading or deserializing the body fails.
pub struct JsonBody<U> {
    length: Option<usize>,
    err: Option<Error<JsonPayloadError>>,
    fut: Option<ReadBody>,
    config: Cfg<ClientConfig>,
    _t: PhantomData<U>,
}

impl<U> JsonBody<U>
where
    U: DeserializeOwned,
{
    #[must_use]
    /// Creates a JSON body future for `res`.
    ///
    /// This takes the response payload unless the content type is not JSON. If
    /// the payload was already taken, the future fails with
    /// [`PayloadError::AlreadyTaken`].
    pub fn new(res: &ClientResponse) -> Self {
        let config = res.config.clone();

        // check content-type
        let json = if let Ok(Some(mime)) = res.mime_type() {
            mime.subtype() == mime::JSON || mime.suffix() == Some(mime::JSON)
        } else {
            false
        };
        if !json {
            let err =
                Some(Error::from(JsonPayloadError::ContentType).with_service(config.service()));
            return JsonBody {
                err,
                config,
                length: None,
                fut: None,
                _t: PhantomData,
            };
        }

        let len = match content_length(res) {
            Ok(len) => len,
            Err(e) => {
                return JsonBody {
                    err: Some(
                        Error::from(JsonPayloadError::Payload(e)).with_service(config.service()),
                    ),
                    config,
                    length: None,
                    fut: None,
                    _t: PhantomData,
                };
            }
        };

        let Some(payload) = res.payload.take() else {
            return JsonBody {
                err: Some(
                    Error::from(JsonPayloadError::Payload(ClientPayloadError(
                        PayloadError::AlreadyTaken,
                    )))
                    .with_service(config.service()),
                ),
                config,
                length: None,
                fut: None,
                _t: PhantomData,
            };
        };

        JsonBody {
            config,
            length: len,
            err: None,
            fut: Some(ReadBody::new(
                payload,
                res.config.response_payload_limit(),
                res.config.response_payload_timeout(),
                res.config.clone(),
            )),
            _t: PhantomData,
        }
    }

    #[must_use]
    /// Sets the maximum buffered payload size.
    ///
    /// The default is 256 KiB. A value of zero disables the limit.
    pub fn limit(mut self, limit: usize) -> Self {
        if let Some(ref mut fut) = self.fut {
            fut.limit = limit;
        }
        self
    }

    #[must_use]
    /// Sets the timeout for reading the complete payload.
    ///
    /// The default is 10 seconds. A zero duration disables the timeout.
    pub fn timeout<T: Into<Millis>>(mut self, to: T) -> Self {
        if let Some(ref mut fut) = self.fut {
            fut.timeout.reset(to.into());
        }
        self
    }
}

impl<U> Unpin for JsonBody<U> where U: DeserializeOwned {}

impl<U> Future for JsonBody<U>
where
    U: DeserializeOwned,
{
    type Output = Result<U, Error<JsonPayloadError>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(err) = self.err.take() {
            return Poll::Ready(Err(err));
        }

        if let Some(len) = self.length.take() {
            let limit = self.fut.as_ref().unwrap().limit;
            if limit > 0 && len > limit {
                return Poll::Ready(Err(Error::from(JsonPayloadError::Payload(
                    ClientPayloadError(PayloadError::Overflow),
                ))
                .with_service(self.config.service())));
            }
        }

        let this = self.get_mut();
        let body = match Pin::new(&mut *this.fut.as_mut().unwrap()).poll(cx) {
            Poll::Ready(result) => result.map_err(|e| e.map(JsonPayloadError::from))?,
            Poll::Pending => return Poll::Pending,
        };
        Poll::Ready(serde_json::from_slice::<U>(&body).map_err(|e| {
            Error::from(JsonPayloadError::from(e)).with_service(this.config.service())
        }))
    }
}

/// Parses the response `Content-Length` header.
fn content_length(res: &ClientResponse) -> Result<Option<usize>, ClientPayloadError> {
    res.headers()
        .get(&CONTENT_LENGTH)
        .map(|l| {
            l.to_str()
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .ok_or(ClientPayloadError(PayloadError::UnknownLength))
        })
        .transpose()
}

#[derive(Debug)]
struct ReadBody {
    stream: Payload,
    buf: BytesMut,
    limit: usize,
    timeout: Deadline,
    config: Cfg<ClientConfig>,
}

impl ReadBody {
    fn new(stream: Payload, limit: usize, timeout: Millis, config: Cfg<ClientConfig>) -> Self {
        Self {
            stream,
            limit,
            config,
            buf: BytesMut::with_capacity(std::cmp::min(limit, 32768)),
            timeout: Deadline::new(timeout),
        }
    }
}

impl Future for ReadBody {
    type Output = Result<Bytes, Error<ClientPayloadError>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        loop {
            return match Pin::new(&mut this.stream).poll_next(cx) {
                Poll::Ready(Some(Ok(chunk))) => {
                    if this.limit > 0 && (this.buf.len() + chunk.len()) > this.limit {
                        Poll::Ready(Err(Error::from(ClientPayloadError(PayloadError::Overflow))
                            .with_service(this.config.service())))
                    } else {
                        this.buf.extend_from_slice(&chunk);
                        continue;
                    }
                }
                Poll::Ready(None) => Poll::Ready(Ok(take_trimmed(&mut this.buf))),
                Poll::Ready(Some(Err(err))) => Poll::Ready(Err(Error::from(ClientPayloadError(
                    err,
                ))
                .with_service(this.config.service()))),
                Poll::Pending => {
                    if this.timeout.poll_elapsed(cx).is_ready() {
                        Poll::Ready(Err(Error::from(ClientPayloadError(
                            PayloadError::Incomplete(Some(std::io::Error::new(
                                std::io::ErrorKind::TimedOut,
                                "Operation timed out",
                            ))),
                        ))
                        .with_service(this.config.service())))
                    } else {
                        Poll::Pending
                    }
                }
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};

    use super::*;
    use crate::client::{error::ClientPayloadError, test::TestResponse};
    use crate::http::header;

    /// STATEMENT: Body collection validates length and size limits and returns a compact payload.
    ///
    /// HOW:
    /// - Collect malformed-length, oversized, limited, and valid bodies and inspect errors or bytes.
    /// - Trim a collected large body and verify its data pointer stays unchanged.
    #[crate::rt_test]
    async fn test_body() {
        let req = TestResponse::with_header(header::CONTENT_LENGTH, "xxxx").build();
        match &*req.body().await.err().unwrap().into_error() {
            PayloadError::UnknownLength => (),
            _ => unreachable!("error"),
        }

        let req = TestResponse::with_header(header::CONTENT_LENGTH, "1000000").build();
        match &*req.body().await.err().unwrap().into_error() {
            PayloadError::Overflow => (),
            _ => unreachable!("error"),
        }

        let req = TestResponse::builder()
            .set_payload(Bytes::from_static(b"test"))
            .build();
        assert_eq!(req.body().await.ok().unwrap(), Bytes::from_static(b"test"));

        let req = TestResponse::builder()
            .set_payload(Bytes::from_static(b"11111111111111"))
            .build();
        match &*req.body().limit(5).await.err().unwrap().into_error() {
            PayloadError::Overflow => (),
            _ => unreachable!("error"),
        }

        // the body does not keep the unused part of the buffer
        let req = TestResponse::builder()
            .set_payload(Bytes::from(vec![b'x'; 1000]))
            .build();
        let mut body = req.body().await.ok().unwrap();
        let ptr = body.as_ptr();
        body.trimdown();
        assert_eq!(body, [b'x'; 1000][..]);
        assert_eq!(body.as_ptr(), ptr, "the body has no unused space");
    }

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct MyObject {
        name: String,
    }

    fn json_eq(err: &JsonPayloadError, other: &JsonPayloadError) -> bool {
        match err {
            JsonPayloadError::Payload(ClientPayloadError(PayloadError::Overflow)) => {
                matches!(
                    other,
                    JsonPayloadError::Payload(ClientPayloadError(PayloadError::Overflow))
                )
            }
            JsonPayloadError::Payload(ClientPayloadError(PayloadError::UnknownLength)) => {
                matches!(
                    other,
                    JsonPayloadError::Payload(ClientPayloadError(PayloadError::UnknownLength))
                )
            }
            JsonPayloadError::ContentType => matches!(other, JsonPayloadError::ContentType),
            _ => false,
        }
    }

    /// STATEMENT: JSON collection validates content type and payload length before deserialization.
    ///
    /// HOW:
    /// - Exercise absent/wrong media types, oversized and invalid lengths, and a valid object.
    /// - Compare error categories and the successfully deserialized value.
    #[crate::rt_test]
    async fn test_json_body() {
        let req = TestResponse::builder().build();
        let json = JsonBody::<MyObject>::new(&req).await;
        assert!(json_eq(
            &json.err().unwrap(),
            &JsonPayloadError::ContentType
        ));

        let req = TestResponse::builder()
            .header(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("application/text"),
            )
            .build();
        let json = JsonBody::<MyObject>::new(&req).await;
        assert!(json_eq(
            &json.err().unwrap(),
            &JsonPayloadError::ContentType
        ));

        let req = TestResponse::builder()
            .header(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("application/json"),
            )
            .header(
                header::CONTENT_LENGTH,
                header::HeaderValue::from_static("10000"),
            )
            .build();

        let json = JsonBody::<MyObject>::new(&req).limit(100).await;
        assert!(json_eq(
            &json.err().unwrap(),
            &JsonPayloadError::Payload(ClientPayloadError(PayloadError::Overflow))
        ));

        let req = TestResponse::with_header(header::CONTENT_TYPE, "application/json")
            .header(header::CONTENT_LENGTH, "xxxx")
            .set_payload(Bytes::from_static(b"{\"name\": \"test\"}"))
            .build();
        let json = JsonBody::<MyObject>::new(&req).await;
        assert!(json_eq(
            &json.err().unwrap(),
            &JsonPayloadError::Payload(ClientPayloadError(PayloadError::UnknownLength))
        ));

        let req = TestResponse::builder()
            .header(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("application/json"),
            )
            .header(
                header::CONTENT_LENGTH,
                header::HeaderValue::from_static("16"),
            )
            .set_payload(Bytes::from_static(b"{\"name\": \"test\"}"))
            .build();

        let json = JsonBody::<MyObject>::new(&req).await;
        assert_eq!(
            json.ok().unwrap(),
            MyObject {
                name: "test".to_owned()
            }
        );
    }

    fn pending_payload() -> Payload {
        Payload::Stream(Box::pin(futures_util::stream::pending()))
    }

    fn error_payload() -> Payload {
        Payload::Stream(Box::pin(futures_util::stream::iter([
            Ok(Bytes::from_static(b"{")),
            Err(PayloadError::Incomplete(None)),
        ])))
    }

    fn is_timeout(err: &PayloadError) -> bool {
        matches!(err, PayloadError::Incomplete(Some(e)) if e.kind() == std::io::ErrorKind::TimedOut)
    }

    /// STATEMENT: Body futures propagate timeout and stream errors and take exclusive ownership of the payload.
    ///
    /// HOW:
    /// - Collect pending and failing streams, then create two body futures for one response.
    /// - Check the errors, verify the second future fails with `PayloadError::AlreadyTaken`, and
    ///   verify the first future still receives the original bytes.
    #[crate::rt_test]
    async fn test_body_timeout_and_error() {
        let res = TestResponse::builder().build();
        res.set_payload(pending_payload());
        let err = res.body().timeout(Millis(1)).await.unwrap_err();
        assert!(is_timeout(&err.into_error().0));

        let res = TestResponse::builder().build();
        res.set_payload(error_payload());
        let err = res.body().await.unwrap_err();
        assert!(matches!(err.into_error().0, PayloadError::Incomplete(None)));

        // the payload is taken by the first body future
        let res = TestResponse::builder()
            .set_payload(b"data".as_ref())
            .build();
        let body = res.body();
        let err = res.body().await.unwrap_err();
        assert!(matches!(err.into_error().0, PayloadError::AlreadyTaken));
        assert_eq!(body.await.unwrap(), Bytes::from_static(b"data"));
    }

    /// STATEMENT: JSON collection distinguishes stream and deserialize failures and accepts JSON syntax suffixes.
    ///
    /// HOW:
    /// - Use pending, failing, and type-invalid payloads and verify their error variants.
    /// - Decode an `application/problem+json` response and check its object field.
    #[crate::rt_test]
    async fn test_json_timeout_and_errors() {
        let res = TestResponse::with_header(header::CONTENT_TYPE, "application/json").build();
        res.set_payload(pending_payload());
        let err = res.json::<MyObject>().timeout(Millis(1)).await.unwrap_err();
        let JsonPayloadError::Payload(ClientPayloadError(err)) = &*err else {
            panic!("{err:?}")
        };
        assert!(is_timeout(err));

        let res = TestResponse::with_header(header::CONTENT_TYPE, "application/json").build();
        res.set_payload(error_payload());
        let err = res.json::<MyObject>().await.unwrap_err();
        assert!(matches!(
            &*err,
            JsonPayloadError::Payload(ClientPayloadError(PayloadError::Incomplete(None)))
        ));

        let res = TestResponse::with_header(header::CONTENT_TYPE, "application/json")
            .set_payload(b"{\"name\": 1}".as_ref())
            .build();
        let err = res.json::<MyObject>().await.unwrap_err();
        assert!(matches!(&*err, JsonPayloadError::Deserialize(Some(_))));

        // a structured syntax suffix is json
        let res = TestResponse::with_header(header::CONTENT_TYPE, "application/problem+json")
            .set_payload(b"{\"name\": \"test\"}".as_ref())
            .build();
        assert_eq!(res.json::<MyObject>().await.unwrap().name, "test");
    }

    /// STATEMENT: Response streaming yields payload chunks and errors, and reports a taken payload once
    /// before ending.
    ///
    /// HOW:
    /// - Poll successful and failing response streams and inspect their items.
    /// - Take a payload twice and verify the second take is empty, then verify the stream yields one
    ///   `PayloadError::AlreadyTaken` error followed by `None`.
    #[crate::rt_test]
    async fn test_response_stream() {
        use futures_util::StreamExt;

        let mut res = TestResponse::builder()
            .set_payload(b"data".as_ref())
            .build();
        assert_eq!(res.next().await.unwrap().unwrap(), "data");
        assert!(res.next().await.is_none());

        let mut res = TestResponse::builder().build();
        res.set_payload(error_payload());
        assert_eq!(res.next().await.unwrap().unwrap(), "{");
        let err = res.next().await.unwrap().unwrap_err();
        assert!(matches!(err.into_error().0, PayloadError::Incomplete(None)));

        // the payload is taken
        let mut res = TestResponse::builder()
            .set_payload(b"data".as_ref())
            .build();
        assert!(matches!(
            res.take_payload(),
            Payload::Stream(_) | Payload::H1(_)
        ));
        assert!(matches!(res.take_payload(), Payload::None));
        let err = res.next().await.unwrap().unwrap_err();
        assert!(matches!(err.into_error().0, PayloadError::AlreadyTaken));
        assert!(res.next().await.is_none());
        assert!(res.next().await.is_none());
    }

    fn is_taken(err: Error<ClientPayloadError>) -> bool {
        matches!(err.into_error().0, PayloadError::AlreadyTaken)
    }

    fn is_json_taken(err: &JsonPayloadError) -> bool {
        matches!(
            err,
            JsonPayloadError::Payload(ClientPayloadError(PayloadError::AlreadyTaken))
        )
    }

    /// STATEMENT: Reading a client response body after its payload was taken fails with
    /// `PayloadError::AlreadyTaken` instead of returning an empty body, while real empty bodies,
    /// JSON content-type rejection, buffer-once parsing and `set_payload()` keep working.
    ///
    /// HOW:
    /// - Drop an unpolled `body()` future and assert later `body()`, `json()` and stream reads
    ///   report `AlreadyTaken`, with the stream ending afterwards.
    /// - Read a body twice and a response without a body twice; assert the first reads return the
    ///   data or empty bytes and the second reads fail.
    /// - Fail `json()` on malformed JSON and assert `body()` then reports `AlreadyTaken`.
    /// - Reject a `text/plain` response in `json()` and assert `body()` still returns all bytes.
    /// - Buffer a malformed JSON body once with `body()`, assert `serde_json::from_slice()` fails
    ///   while the retained bytes are intact.
    /// - Install a new payload with `set_payload()` after the stream reported the error and assert
    ///   both `body()` and the stream can read again.
    #[crate::rt_test]
    async fn test_taken_payload_is_reported() {
        use futures_util::StreamExt;

        let mut res = TestResponse::with_header(header::CONTENT_TYPE, "application/json")
            .set_payload(b"{\"name\": \"test\"}".as_ref())
            .build();
        drop(res.body());
        assert!(is_taken(res.body().await.unwrap_err()));
        assert!(is_json_taken(&res.json::<MyObject>().await.unwrap_err()));
        assert!(is_taken(res.next().await.unwrap().unwrap_err()));
        assert!(res.next().await.is_none());

        let res = TestResponse::builder()
            .set_payload(b"data".as_ref())
            .build();
        assert_eq!(res.body().await.unwrap(), "data");
        assert!(is_taken(res.body().await.unwrap_err()));

        let res = TestResponse::builder().build();
        assert_eq!(res.body().await.unwrap(), Bytes::new());
        assert!(is_taken(res.body().await.unwrap_err()));

        let res = TestResponse::with_header(header::CONTENT_TYPE, "application/json")
            .set_payload(b"{\"name\":".as_ref())
            .build();
        let err = res.json::<MyObject>().await.unwrap_err();
        assert!(matches!(&*err, JsonPayloadError::Deserialize(Some(_))));
        assert!(is_taken(res.body().await.unwrap_err()));

        let res = TestResponse::with_header(header::CONTENT_TYPE, "text/plain")
            .set_payload(b"{\"name\": \"test\"}".as_ref())
            .build();
        let err = res.json::<MyObject>().await.unwrap_err();
        assert!(matches!(&*err, JsonPayloadError::ContentType));
        assert_eq!(res.body().await.unwrap(), "{\"name\": \"test\"}");

        let res = TestResponse::with_header(header::CONTENT_TYPE, "application/json")
            .set_payload(b"{\"name\":".as_ref())
            .build();
        let body = res.body().await.unwrap();
        assert!(serde_json::from_slice::<MyObject>(&body).is_err());
        assert_eq!(body, "{\"name\":");

        let mut res = TestResponse::builder().build();
        drop(res.take_payload());
        assert!(is_taken(res.next().await.unwrap().unwrap_err()));
        res.set_payload(Payload::from(crate::channel::bstream::empty(Some(
            Bytes::from_static(b"again"),
        ))));
        assert_eq!(res.next().await.unwrap().unwrap(), "again");
        res.set_payload(Payload::from(crate::channel::bstream::empty(Some(
            Bytes::from_static(b"again"),
        ))));
        assert_eq!(res.body().await.unwrap(), "again");
        assert!(is_taken(res.next().await.unwrap().unwrap_err()));
    }

    /// STATEMENT: A malformed `Set-Cookie` header is skipped without hiding valid cookies.
    ///
    /// HOW:
    /// - Build a client response with `Set-Cookie` headers `garbage`, a non-UTF-8 value, and
    ///   `sid=ok; Path=/p`.
    /// - Confirm `cookies` returns only `sid` and `cookie("sid")` keeps value `ok` and path `/p`.
    #[cfg(feature = "cookie")]
    #[test]
    fn test_cookies_malformed_header() {
        let res = TestResponse::with_header(header::SET_COOKIE, "garbage")
            .header(
                header::SET_COOKIE,
                HeaderValue::from_bytes(b"c=\xff").unwrap(),
            )
            .header(header::SET_COOKIE, "sid=ok; Path=/p")
            .build();
        assert_eq!(res.cookies().unwrap().len(), 1);
        let cookie = res.cookie("sid").unwrap();
        assert_eq!((cookie.value(), cookie.path()), ("ok", Some("/p")));
    }

    /// STATEMENT: Response metadata mutations are visible through accessors and debug output.
    ///
    /// HOW:
    /// - Set version, headers, and an extension, then inspect getters and the formatted response.
    #[test]
    fn test_response_accessors() {
        let mut res = TestResponse::with_header(header::CONTENT_TYPE, "text/plain")
            .version(Version::HTTP_2)
            .build();
        res.headers_mut()
            .insert(header::SERVER, HeaderValue::from_static("test"));
        assert_eq!(res.header(header::SERVER).unwrap(), "test");
        assert_eq!(res.version(), Version::HTTP_2);

        res.extensions_mut().insert(10u32);
        assert_eq!(res.extensions().get::<u32>(), Some(&10));

        let s = format!("{res:?}");
        assert!(s.contains("ClientResponse HTTP/2.0 200 OK"), "{s}");
        assert!(s.contains("\"server\": \"test\""), "{s}");
    }
}
