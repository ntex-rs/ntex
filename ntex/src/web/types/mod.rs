//! Extractor types

pub(in crate::web) mod form;
pub(in crate::web) mod json;
mod path;
pub(in crate::web) mod payload;
mod query;

pub use self::form::{Form, FormConfig};
pub use self::json::{Json, JsonConfig};
pub use self::path::Path;
pub use self::payload::{Payload, PayloadConfig};
pub use self::query::Query;

use std::{future::Future, pin::Pin, task::Context, task::Poll};

#[cfg(feature = "compress")]
use crate::http::encoding::Decoder;
use crate::http::{Payload as HttpPayload, error::PayloadError, helpers::take_trimmed};
use crate::util::{Bytes, BytesMut, Stream};
use crate::web::HttpRequest;

/// Request body stream, decoded according to `Content-Encoding`.
enum BodyStream {
    Plain(HttpPayload),
    #[cfg(feature = "compress")]
    Decoded(Box<Decoder<HttpPayload>>),
}

impl BodyStream {
    /// Takes the request payload, a body without `Content-Encoding` is not
    /// passed through a decoder.
    fn new(req: &HttpRequest, payload: &mut HttpPayload) -> Self {
        #[cfg(feature = "compress")]
        if req
            .headers()
            .contains_key(&crate::http::header::CONTENT_ENCODING)
        {
            return BodyStream::Decoded(Box::new(Decoder::from_headers(
                payload.take(),
                req.headers(),
            )));
        }
        #[cfg(not(feature = "compress"))]
        let _ = req;
        BodyStream::Plain(payload.take())
    }
}

impl Stream for BodyStream {
    type Item = Result<Bytes, PayloadError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.get_mut() {
            BodyStream::Plain(pl) => Pin::new(pl).poll_next(cx),
            #[cfg(feature = "compress")]
            BodyStream::Decoded(pl) => Pin::new(pl.as_mut()).poll_next(cx),
        }
    }
}

/// Failure to read the complete body.
enum ReadBodyError {
    /// The body size exceeds the limit
    Overflow(usize),
    Payload(PayloadError),
}

/// Future that reads the complete body, failing with
/// `ReadBodyError::Overflow(size)` once `size > limit`.
///
/// A single-chunk body is copied only if it is a part of a larger buffer,
/// which it would otherwise keep alive.
struct ReadBody<S> {
    stream: S,
    limit: usize,
    length: Option<usize>,
    first: Option<Bytes>,
    body: Option<BytesMut>,
}

impl<S> ReadBody<S> {
    fn new(stream: S, limit: usize, length: Option<usize>) -> Self {
        ReadBody {
            stream,
            limit,
            length,
            first: None,
            body: None,
        }
    }
}

impl ReadBody<BodyStream> {
    fn from_request(req: &HttpRequest, payload: &mut HttpPayload, limit: usize) -> Self {
        Self::new(BodyStream::new(req, payload), limit, None)
    }

    fn length(mut self, length: Option<usize>) -> Self {
        self.length = length;
        self
    }
}

impl<S> Future for ReadBody<S>
where
    S: Stream<Item = Result<Bytes, PayloadError>> + Unpin,
{
    type Output = Result<Bytes, ReadBodyError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        loop {
            let chunk = match Pin::new(&mut this.stream).poll_next(cx) {
                Poll::Ready(Some(Ok(chunk))) => chunk,
                Poll::Ready(Some(Err(err))) => {
                    return Poll::Ready(Err(ReadBodyError::Payload(err)));
                }
                Poll::Ready(None) => {
                    let body = if let Some(mut body) = this.body.take() {
                        take_trimmed(&mut body)
                    } else if let Some(mut first) = this.first.take() {
                        first.trimdown();
                        first
                    } else {
                        Bytes::new()
                    };
                    return Poll::Ready(Ok(body));
                }
                Poll::Pending => return Poll::Pending,
            };

            if let Some(body) = this.body.as_mut() {
                let size = body.len() + chunk.len();
                if size > this.limit {
                    return Poll::Ready(Err(ReadBodyError::Overflow(size)));
                }
                body.extend_from_slice(&chunk);
            } else if let Some(first) = this.first.take() {
                let size = first.len() + chunk.len();
                if size > this.limit {
                    return Poll::Ready(Err(ReadBodyError::Overflow(size)));
                }
                let mut body =
                    BytesMut::with_capacity(this.length.unwrap_or(0).clamp(size, this.limit));
                body.extend_from_slice(&first);
                body.extend_from_slice(&chunk);
                this.body = Some(body);
            } else {
                if chunk.len() > this.limit {
                    return Poll::Ready(Err(ReadBodyError::Overflow(chunk.len())));
                }
                this.first = Some(chunk);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, pin::Pin, task::Context, task::Poll};

    use super::*;

    struct Chunks(VecDeque<Result<Bytes, PayloadError>>);

    impl Stream for Chunks {
        type Item = Result<Bytes, PayloadError>;

        fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Ready(self.0.pop_front())
        }
    }

    fn chunks(items: &[&'static [u8]]) -> Chunks {
        Chunks(items.iter().map(|b| Ok(Bytes::from_static(b))).collect())
    }

    /// STATEMENT: Body collection preserves contents, avoids unnecessary retention, and enforces limits.
    ///
    /// HOW:
    /// - Check a static single chunk is reused, an empty stream is empty, and multiple chunks concatenate.
    /// - Assert oversized backing storage is copied away and an accumulated body has no unused capacity.
    /// - Exceed a six-byte limit with different chunk layouts and assert overflow reports seven bytes.
    #[crate::rt_test]
    async fn test_read_body() {
        let data: &'static [u8] = b"single chunk";
        let body = ReadBody::new(chunks(&[data]), 64, None).await.ok().unwrap();
        assert_eq!(
            body.as_ptr(),
            data.as_ptr(),
            "single chunk must not be copied"
        );

        let body = ReadBody::new(chunks(&[]), 64, None).await.ok().unwrap();
        assert!(body.is_empty());

        let body = ReadBody::new(chunks(&[b"ab", b"cd", b"ef"]), 6, Some(6))
            .await
            .ok()
            .unwrap();
        assert_eq!(body, Bytes::from_static(b"abcdef"));

        // the body does not keep the larger buffer it is a part of
        let page = Bytes::from(vec![b'x'; 32 * 1024]);
        let stream = Chunks([Ok(page.slice(..1024))].into());
        let body = ReadBody::new(stream, 64 * 1024, None).await.ok().unwrap();
        assert_eq!(body, page.slice(..1024));
        assert_ne!(body.as_ptr(), page.as_ptr());

        // nor the unused part of the buffer it is read into
        let chunk = Bytes::from(vec![b'x'; 1000]);
        let stream = Chunks(std::iter::repeat_n(Ok(chunk), 33).collect());
        let body = ReadBody::new(stream, 64 * 1024, None).await.ok().unwrap();
        assert_eq!(body.len(), 33 * 1000);
        let mut copy = body.clone();
        copy.trimdown();
        assert_eq!(copy.as_ptr(), body.as_ptr(), "the body has no unused space");

        for items in [
            &[&b"abcdefg"[..]][..],
            &[b"abcd", b"efg"],
            &[b"ab", b"cd", b"efg"],
        ] {
            let res = ReadBody::new(chunks(items), 6, None).await;
            assert!(matches!(res, Err(ReadBodyError::Overflow(7))));
        }
    }
}
