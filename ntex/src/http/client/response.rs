use std::cell::{Ref, RefMut};
use std::task::{Context, Poll};
use std::{fmt, future::Future, marker::PhantomData, mem, pin::Pin};

use bytes::{Bytes, BytesMut};
use futures_core::Stream;
use serde::de::DeserializeOwned;

#[cfg(feature = "cookie")]
use coo_kie::{Cookie, ParseError as CookieParseError};

use crate::http::error::PayloadError;
use crate::http::header::{AsName, HeaderValue, CONTENT_LENGTH};
use crate::http::{HeaderMap, StatusCode, Version};
use crate::http::{HttpMessage, Payload, ResponseHead};
use crate::util::Extensions;

use super::error::JsonPayloadError;

/// Client Response
pub struct ClientResponse {
    pub(crate) head: ResponseHead,
    pub(crate) payload: Payload,
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
                let s = std::str::from_utf8(hdr.as_bytes())
                    .map_err(CookieParseError::from)?;
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
    /// Create new Request instance
    pub(crate) fn new(head: ResponseHead, payload: Payload) -> Self {
        ClientResponse { head, payload }
    }

    #[inline]
    pub(crate) fn head(&self) -> &ResponseHead {
        &self.head
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
    /// Returns request's headers.
    pub fn headers(&self) -> &HeaderMap {
        &self.head().headers
    }

    /// Set a body and return previous body value
    pub fn set_payload(&mut self, payload: Payload) {
        self.payload = payload;
    }

    /// Get response's payload
    pub fn take_payload(&mut self) -> Payload {
        mem::take(&mut self.payload)
    }

    /// Request extensions
    #[inline]
    pub fn extensions(&self) -> Ref<'_, Extensions> {
        self.head().extensions()
    }

    /// Mutable reference to a the request's extensions
    #[inline]
    pub fn extensions_mut(&self) -> RefMut<'_, Extensions> {
        self.head().extensions_mut()
    }
}

impl ClientResponse {
    /// Loads http response's body.
    pub fn body(&mut self) -> MessageBody {
        MessageBody::new(self)
    }

    /// Loads and parse `application/json` encoded body.
    /// Return `JsonBody<T>` future. It resolves to a `T` value.
    ///
    /// Returns error:
    ///
    /// * content type is not `application/json`
    /// * content length is greater than 256k
    pub fn json<T: DeserializeOwned>(&mut self) -> JsonBody<T> {
        JsonBody::new(self)
    }
}

impl Stream for ClientResponse {
    type Item = Result<Bytes, PayloadError>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.get_mut().payload).poll_next(cx)
    }
}

impl fmt::Debug for ClientResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "\nClientResponse {:?} {}", self.version(), self.status(),)?;
        writeln!(f, "  headers:")?;
        for (key, val) in self.headers().iter() {
            writeln!(f, "    {:?}: {:?}", key, val)?;
        }
        Ok(())
    }
}

/// Future that resolves to a complete http message body.
pub struct MessageBody {
    length: Option<usize>,
    err: Option<PayloadError>,
    fut: Option<ReadBody>,
}

impl MessageBody {
    /// Create `MessageBody` for request.
    pub fn new(res: &mut ClientResponse) -> MessageBody {
        let mut len = None;
        if let Some(l) = res.headers().get(&CONTENT_LENGTH) {
            if let Ok(s) = l.to_str() {
                if let Ok(l) = s.parse::<usize>() {
                    len = Some(l)
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
            fut: Some(ReadBody::new(res.take_payload(), 262_144)),
        }
    }

    /// Change max size of payload. By default max size is 256Kb
    pub fn limit(mut self, limit: usize) -> Self {
        if let Some(ref mut fut) = self.fut {
            fut.limit = limit;
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
            if len > this.fut.as_ref().unwrap().limit {
                return Poll::Ready(Err(PayloadError::Overflow));
            }
        }

        Pin::new(&mut this.fut.as_mut().unwrap()).poll(cx)
    }
}

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
    /// Create `JsonBody` for request.
    pub fn new(req: &mut ClientResponse) -> Self {
        // check content-type
        let json = if let Ok(Some(mime)) = req.mime_type() {
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
        if let Some(l) = req.headers().get(&CONTENT_LENGTH) {
            if let Ok(s) = l.to_str() {
                if let Ok(l) = s.parse::<usize>() {
                    len = Some(l)
                }
            }
        }

        JsonBody {
            length: len,
            err: None,
            fut: Some(ReadBody::new(req.take_payload(), 65536)),
            _t: PhantomData,
        }
    }

    /// Change max size of payload. By default max size is 64Kb
    pub fn limit(mut self, limit: usize) -> Self {
        if let Some(ref mut fut) = self.fut {
            fut.limit = limit;
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
            if len > self.fut.as_ref().unwrap().limit {
                return Poll::Ready(Err(JsonPayloadError::Payload(
                    PayloadError::Overflow,
                )));
            }
        }

        let body = match Pin::new(&mut self.get_mut().fut.as_mut().unwrap()).poll(cx) {
            Poll::Ready(result) => result?,
            Poll::Pending => return Poll::Pending,
        };
        Poll::Ready(serde_json::from_slice::<U>(&body).map_err(JsonPayloadError::from))
    }
}

struct ReadBody {
    stream: Payload,
    buf: BytesMut,
    limit: usize,
}

impl ReadBody {
    fn new(stream: Payload, limit: usize) -> Self {
        Self {
            stream,
            buf: BytesMut::with_capacity(std::cmp::min(limit, 32768)),
            limit,
        }
    }
}

impl Future for ReadBody {
    type Output = Result<Bytes, PayloadError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        loop {
            return match Pin::new(&mut this.stream).poll_next(cx)? {
                Poll::Ready(Some(chunk)) => {
                    if (this.buf.len() + chunk.len()) > this.limit {
                        Poll::Ready(Err(PayloadError::Overflow))
                    } else {
                        this.buf.extend_from_slice(&chunk);
                        continue;
                    }
                }
                Poll::Ready(None) => Poll::Ready(Ok(this.buf.split().freeze())),
                Poll::Pending => Poll::Pending,
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    use crate::http::client::test::TestResponse;
    use crate::http::header;

    #[crate::rt_test]
    async fn test_body() {
        let mut req = TestResponse::with_header(header::CONTENT_LENGTH, "xxxx").finish();
        match req.body().await.err().unwrap() {
            PayloadError::UnknownLength => (),
            _ => unreachable!("error"),
        }

        let mut req =
            TestResponse::with_header(header::CONTENT_LENGTH, "1000000").finish();
        match req.body().await.err().unwrap() {
            PayloadError::Overflow => (),
            _ => unreachable!("error"),
        }

        let mut req = TestResponse::default()
            .set_payload(Bytes::from_static(b"test"))
            .finish();
        assert_eq!(req.body().await.ok().unwrap(), Bytes::from_static(b"test"));

        let mut req = TestResponse::default()
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

    fn json_eq(err: JsonPayloadError, other: JsonPayloadError) -> bool {
        match err {
            JsonPayloadError::Payload(PayloadError::Overflow) => match other {
                JsonPayloadError::Payload(PayloadError::Overflow) => true,
                _ => false,
            },
            JsonPayloadError::ContentType => match other {
                JsonPayloadError::ContentType => true,
                _ => false,
            },
            _ => false,
        }
    }

    #[crate::rt_test]
    async fn test_json_body() {
        let mut req = TestResponse::default().finish();
        let json = JsonBody::<MyObject>::new(&mut req).await;
        assert!(json_eq(json.err().unwrap(), JsonPayloadError::ContentType));

        let mut req = TestResponse::default()
            .header(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("application/text"),
            )
            .finish();
        let json = JsonBody::<MyObject>::new(&mut req).await;
        assert!(json_eq(json.err().unwrap(), JsonPayloadError::ContentType));

        let mut req = TestResponse::default()
            .header(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("application/json"),
            )
            .header(
                header::CONTENT_LENGTH,
                header::HeaderValue::from_static("10000"),
            )
            .finish();

        let json = JsonBody::<MyObject>::new(&mut req).limit(100).await;
        assert!(json_eq(
            json.err().unwrap(),
            JsonPayloadError::Payload(PayloadError::Overflow)
        ));

        let mut req = TestResponse::default()
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

        let json = JsonBody::<MyObject>::new(&mut req).await;
        assert_eq!(
            json.ok().unwrap(),
            MyObject {
                name: "test".to_owned()
            }
        );
    }
}
