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
    fn cookies(&self) -> Result<Ref<'_, Vec<Cookie<'static>>>, CookieParseError> {
        use crate::http::header::SET_COOKIE;

        struct Cookies(Vec<Cookie<'static>>);

        if self.message_extensions().get::<Cookies>().is_none() {
            let mut cookies = Vec::new();
            for hdr in self.message_headers().get_all(&SET_COOKIE) {
                let s = std::str::from_utf8(hdr.as_bytes()).map_err(CookieParseError::from)?;
                cookies.push(Cookie::parse_encoded(s)?.into_owned());
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
    /// Any unread previous payload is dropped.
    pub fn set_payload(&self, payload: Payload) {
        self.payload.set(Some(payload));
    }

    #[must_use]
    /// Takes the response payload.
    ///
    /// Subsequent calls return an empty payload.
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
        } else {
            Poll::Ready(None)
        }
    }
}

impl From<ServiceResponse> for ClientResponse {
    fn from(res: ServiceResponse) -> Self {
        Self {
            head: res.head,
            payload: Cell::new(Some(res.payload)),
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
    /// This takes the response payload. Creating another body future from the
    /// same response produces an empty body.
    pub fn new(res: &ClientResponse) -> MessageBody {
        let config = res.config.clone();

        let len = match content_length(res) {
            Ok(len) => len,
            Err(e) => return Self::err(Error::from(e).with_service(config.service()), config),
        };

        MessageBody {
            config,
            length: len,
            err: None,
            fut: Some(ReadBody::new(
                res.take_payload(),
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
    /// This takes the response payload. Creating another body future from the
    /// same response produces an empty body.
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

        JsonBody {
            config,
            length: len,
            err: None,
            fut: Some(ReadBody::new(
                res.take_payload(),
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
        assert_eq!(res.body().await.unwrap(), Bytes::new());
        assert_eq!(body.await.unwrap(), Bytes::from_static(b"data"));
    }

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
        assert!(res.next().await.is_none());
    }

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
