use std::cell::{Cell, Ref, RefMut};
use std::task::{Context, Poll};
use std::{fmt, future::Future, marker::PhantomData, pin::Pin, rc::Rc};

use serde::de::DeserializeOwned;

#[cfg(feature = "cookie")]
use coo_kie::{Cookie, ParseError as CookieParseError};

use crate::http::error::PayloadError;
use crate::http::header::{AsName, CONTENT_LENGTH, HeaderValue};
use crate::http::{HeaderMap, HttpMessage, Payload, ResponseHead, StatusCode, Version};
use crate::time::{Deadline, Millis};
use crate::util::{Bytes, BytesMut, Extensions, Stream};

use super::{ClientConfig, error::JsonPayloadError};

/// Client Response
pub struct ClientResponse {
    pub(crate) head: ResponseHead,
    pub(crate) payload: Cell<Option<Payload>>,
    config: Rc<ClientConfig>,
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
    /// Load request cookies.
    fn cookies(&self) -> Result<Ref<'_, Vec<Cookie<'static>>>, CookieParseError> {
        use crate::http::header::SET_COOKIE;

        struct Cookies(Vec<Cookie<'static>>);

        if self.message_extensions().get::<Cookies>().is_none() {
            let mut cookies = Vec::new();
            for hdr in self.message_headers().get_all(&SET_COOKIE) {
                let s =
                    std::str::from_utf8(hdr.as_bytes()).map_err(CookieParseError::from)?;
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
    /// Create new client response instance
    #[doc(hidden)]
    pub fn new(head: ResponseHead, payload: Payload, config: Rc<ClientConfig>) -> Self {
        ClientResponse {
            head,
            config,
            payload: Cell::new(Some(payload)),
        }
    }

    #[cfg(feature = "ws")]
    pub(crate) fn with_empty_payload(head: ResponseHead, config: Rc<ClientConfig>) -> Self {
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

    /// Read the Request Version.
    #[inline]
    pub fn version(&self) -> Version {
        self.head().version
    }

    /// Get the status from the server.
    #[inline]
    pub fn status(&self) -> StatusCode {
        self.head().status
    }

    #[inline]
    /// Returns a reference to the header value.
    pub fn header<N: AsName>(&self, name: N) -> Option<&HeaderValue> {
        self.head().headers.get(name)
    }

    #[inline]
    /// Returns response's headers.
    pub fn headers(&self) -> &HeaderMap {
        &self.head().headers
    }

    #[inline]
    /// Returns mutable response's headers.
    pub fn headers_mut(&mut self) -> &mut HeaderMap {
        &mut self.head_mut().headers
    }

    /// Set a body and return previous body value.
    pub fn set_payload(&self, payload: Payload) {
        self.payload.set(Some(payload));
    }

    #[must_use]
    /// Get response's payload.
    pub fn take_payload(&self) -> Payload {
        if let Some(pl) = self.payload.take() {
            pl
        } else {
            Payload::None
        }
    }

    /// Request extensions.
    #[inline]
    pub fn extensions(&self) -> Ref<'_, Extensions> {
        self.head().extensions()
    }

    /// Mutable reference to a the request's extensions.
    #[inline]
    pub fn extensions_mut(&self) -> RefMut<'_, Extensions> {
        self.head().extensions_mut()
    }
}

impl ClientResponse {
    /// Loads http response's body.
    pub fn body(&self) -> MessageBody {
        MessageBody::new(self)
    }

    /// Loads and parse `application/json` encoded body.
    /// Return `JsonBody<T>` future. It resolves to a `T` value.
    ///
    /// Returns error:
    ///
    /// * content type is not `application/json`
    /// * content length is greater than 256k
    pub fn json<T: DeserializeOwned>(&self) -> JsonBody<T> {
        JsonBody::new(self)
    }
}

impl Stream for ClientResponse {
    type Item = Result<Bytes, PayloadError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Some(mut pl) = self.payload.take() {
            let result = Pin::new(&mut pl).poll_next(cx);
            self.payload.set(Some(pl));
            result
        } else {
            Poll::Ready(None)
        }
    }
}

impl fmt::Debug for ClientResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "\nClientResponse {:?} {}", self.version(), self.status(),)?;
        writeln!(f, "  headers:")?;
        for (key, val) in self.headers() {
            writeln!(f, "    {key:?}: {val:?}")?;
        }
        Ok(())
    }
}

#[derive(Debug)]
/// Future that resolves to a complete http message body.
pub struct MessageBody {
    length: Option<usize>,
    err: Option<PayloadError>,
    fut: Option<ReadBody>,
}

impl MessageBody {
    /// Create `MessageBody` for request.
    pub fn new(res: &ClientResponse) -> MessageBody {
        let mut len = None;
        if let Some(l) = res.headers().get(&CONTENT_LENGTH) {
            if let Ok(s) = l.to_str() {
                if let Ok(l) = s.parse::<usize>() {
                    len = Some(l);
                } else {
                    return Self::err(PayloadError::UnknownLength);
                }
            } else {
                return Self::err(PayloadError::UnknownLength);
            }
        }

        MessageBody {
            length: len,
            err: None,
            fut: Some(ReadBody::new(
                res.take_payload(),
                res.config.response_pl_limit,
                res.config.response_pl_timeout,
            )),
        }
    }

    #[must_use]
    /// Change max size of payload.
    ///
    /// By default max size is 256Kb
    pub fn limit(mut self, limit: usize) -> Self {
        if let Some(ref mut fut) = self.fut {
            fut.limit = limit;
        }
        self
    }

    #[must_use]
    /// Set operation timeout.
    ///
    /// By default timeout is set to 10 seconds. Set 0 millis to disable
    /// timeout.
    pub fn timeout(mut self, to: Millis) -> Self {
        if let Some(ref mut fut) = self.fut {
            fut.timeout.reset(to);
        }
        self
    }

    fn err(e: PayloadError) -> Self {
        MessageBody {
            fut: None,
            err: Some(e),
            length: None,
        }
    }
}

impl Future for MessageBody {
    type Output = Result<Bytes, PayloadError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        if let Some(err) = this.err.take() {
            return Poll::Ready(Err(err));
        }

        if let Some(len) = this.length.take() {
            let limit = this.fut.as_ref().unwrap().limit;
            if limit > 0 && len > limit {
                return Poll::Ready(Err(PayloadError::Overflow));
            }
        }

        Pin::new(&mut this.fut.as_mut().unwrap()).poll(cx)
    }
}

#[derive(Debug)]
/// Response's payload json parser, it resolves to a deserialized `T` value.
///
/// Returns error:
///
/// * content type is not `application/json`
/// * content length is greater than 64k
pub struct JsonBody<U> {
    length: Option<usize>,
    err: Option<JsonPayloadError>,
    fut: Option<ReadBody>,
    _t: PhantomData<U>,
}

impl<U> JsonBody<U>
where
    U: DeserializeOwned,
{
    #[must_use]
    /// Create `JsonBody` for request.
    pub fn new(res: &ClientResponse) -> Self {
        // check content-type
        let json = if let Ok(Some(mime)) = res.mime_type() {
            mime.subtype() == mime::JSON || mime.suffix() == Some(mime::JSON)
        } else {
            false
        };
        if !json {
            return JsonBody {
                length: None,
                fut: None,
                err: Some(JsonPayloadError::ContentType),
                _t: PhantomData,
            };
        }

        let mut len = None;
        if let Some(l) = res.headers().get(&CONTENT_LENGTH)
            && let Ok(s) = l.to_str()
            && let Ok(l) = s.parse::<usize>()
        {
            len = Some(l);
        }

        JsonBody {
            length: len,
            err: None,
            fut: Some(ReadBody::new(
                res.take_payload(),
                res.config.response_pl_limit,
                res.config.response_pl_timeout,
            )),
            _t: PhantomData,
        }
    }

    #[must_use]
    /// Change max size of payload.
    ///
    /// By default max size is 64Kb.
    pub fn limit(mut self, limit: usize) -> Self {
        if let Some(ref mut fut) = self.fut {
            fut.limit = limit;
        }
        self
    }

    #[must_use]
    /// Set operation timeout.
    ///
    /// By default timeout is set to 10 seconds. Set 0 millis to disable
    /// timeout.
    pub fn timeout(mut self, to: Millis) -> Self {
        if let Some(ref mut fut) = self.fut {
            fut.timeout.reset(to);
        }
        self
    }
}

impl<U> Unpin for JsonBody<U> where U: DeserializeOwned {}

impl<U> Future for JsonBody<U>
where
    U: DeserializeOwned,
{
    type Output = Result<U, JsonPayloadError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(err) = self.err.take() {
            return Poll::Ready(Err(err));
        }

        if let Some(len) = self.length.take() {
            let limit = self.fut.as_ref().unwrap().limit;
            if limit > 0 && len > limit {
                return Poll::Ready(Err(JsonPayloadError::Payload(PayloadError::Overflow)));
            }
        }

        let body = match Pin::new(&mut self.get_mut().fut.as_mut().unwrap()).poll(cx) {
            Poll::Ready(result) => result?,
            Poll::Pending => return Poll::Pending,
        };
        Poll::Ready(serde_json::from_slice::<U>(&body).map_err(JsonPayloadError::from))
    }
}

#[derive(Debug)]
struct ReadBody {
    stream: Payload,
    buf: BytesMut,
    limit: usize,
    timeout: Deadline,
}

impl ReadBody {
    fn new(stream: Payload, limit: usize, timeout: Millis) -> Self {
        Self {
            stream,
            limit,
            buf: BytesMut::with_capacity(std::cmp::min(limit, 32768)),
            timeout: Deadline::new(timeout),
        }
    }
}

impl Future for ReadBody {
    type Output = Result<Bytes, PayloadError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        loop {
            return match Pin::new(&mut this.stream).poll_next(cx) {
                Poll::Ready(Some(Ok(chunk))) => {
                    if this.limit > 0 && (this.buf.len() + chunk.len()) > this.limit {
                        Poll::Ready(Err(PayloadError::Overflow))
                    } else {
                        this.buf.extend_from_slice(&chunk);
                        continue;
                    }
                }
                Poll::Ready(None) => Poll::Ready(Ok(this.buf.take())),
                Poll::Ready(Some(Err(err))) => Poll::Ready(Err(err)),
                Poll::Pending => {
                    if this.timeout.poll_elapsed(cx).is_ready() {
                        Poll::Ready(Err(PayloadError::Incomplete(Some(
                            std::io::Error::new(
                                std::io::ErrorKind::TimedOut,
                                "Operation timed out",
                            ),
                        ))))
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
    use super::*;
    use serde::{Deserialize, Serialize};

    use crate::{client::test::TestResponse, http::header};

    #[crate::rt_test]
    async fn test_body() {
        let req = TestResponse::with_header(header::CONTENT_LENGTH, "xxxx").finish();
        match req.body().await.err().unwrap() {
            PayloadError::UnknownLength => (),
            _ => unreachable!("error"),
        }

        let req = TestResponse::with_header(header::CONTENT_LENGTH, "1000000").finish();
        match req.body().await.err().unwrap() {
            PayloadError::Overflow => (),
            _ => unreachable!("error"),
        }

        let req = TestResponse::default()
            .set_payload(Bytes::from_static(b"test"))
            .finish();
        assert_eq!(req.body().await.ok().unwrap(), Bytes::from_static(b"test"));

        let req = TestResponse::default()
            .set_payload(Bytes::from_static(b"11111111111111"))
            .finish();
        match req.body().limit(5).await.err().unwrap() {
            PayloadError::Overflow => (),
            _ => unreachable!("error"),
        }
    }

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct MyObject {
        name: String,
    }

    fn json_eq(err: &JsonPayloadError, other: &JsonPayloadError) -> bool {
        match err {
            JsonPayloadError::Payload(PayloadError::Overflow) => {
                matches!(other, JsonPayloadError::Payload(PayloadError::Overflow))
            }
            JsonPayloadError::ContentType => matches!(other, JsonPayloadError::ContentType),
            _ => false,
        }
    }

    #[crate::rt_test]
    async fn test_json_body() {
        let req = TestResponse::default().finish();
        let json = JsonBody::<MyObject>::new(&req).await;
        assert!(json_eq(
            &json.err().unwrap(),
            &JsonPayloadError::ContentType
        ));

        let req = TestResponse::default()
            .header(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("application/text"),
            )
            .finish();
        let json = JsonBody::<MyObject>::new(&req).await;
        assert!(json_eq(
            &json.err().unwrap(),
            &JsonPayloadError::ContentType
        ));

        let req = TestResponse::default()
            .header(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("application/json"),
            )
            .header(
                header::CONTENT_LENGTH,
                header::HeaderValue::from_static("10000"),
            )
            .finish();

        let json = JsonBody::<MyObject>::new(&req).limit(100).await;
        assert!(json_eq(
            &json.err().unwrap(),
            &JsonPayloadError::Payload(PayloadError::Overflow)
        ));

        let req = TestResponse::default()
            .header(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("application/json"),
            )
            .header(
                header::CONTENT_LENGTH,
                header::HeaderValue::from_static("16"),
            )
            .set_payload(Bytes::from_static(b"{\"name\": \"test\"}"))
            .finish();

        let json = JsonBody::<MyObject>::new(&req).await;
        assert_eq!(
            json.ok().unwrap(),
            MyObject {
                name: "test".to_owned()
            }
        );
    }
}
