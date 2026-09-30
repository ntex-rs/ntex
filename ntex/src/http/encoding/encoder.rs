//! Stream encoder
use std::{fmt, future::Future, io, io::Write, pin::Pin, rc::Rc, task::Context, task::Poll};

use flate2::write::{GzEncoder, ZlibEncoder};

use crate::http::body::{Body, BodySize, MessageBody, ResponseBody};
use crate::http::header::{CONTENT_ENCODING, ContentEncoding, HeaderValue};
use crate::http::{ResponseHead, StatusCode};
use crate::rt::{BlockingResult, spawn_blocking};
use crate::util::{Bytes, dyn_rc_err};

use super::Writer;

const INPLACE: usize = 1024;

/// Response body encoder.
///
/// Compresses a response body with the selected content encoding.
pub struct Encoder<B> {
    eof: bool,
    body: EncoderBody<B>,
    inner: Option<ContentEncoder>,
    fut: Option<BlockingResult<Result<ContentEncoder, io::Error>>>,
}

impl<B: MessageBody> Encoder<B> {
    /// Wrap a response body in an encoder for `encoding`.
    ///
    /// On success the `Content-Encoding` header is set and chunked
    /// transfer-encoding is enabled. The body is returned unchanged if the
    /// encoding is not supported, is `Identity` or `Auto`, if the response
    /// already has a `Content-Encoding` header, if the status is
    /// `101 Switching Protocols` or `204 No Content`, or if the body is empty.
    pub fn response(
        encoding: ContentEncoding,
        head: &mut ResponseHead,
        body: ResponseBody<B>,
    ) -> ResponseBody<B> {
        let can_encode = ContentEncoder::can_encode(encoding)
            && !(head.headers().contains_key(&CONTENT_ENCODING)
                || head.status == StatusCode::SWITCHING_PROTOCOLS
                || head.status == StatusCode::NO_CONTENT
                || encoding == ContentEncoding::Identity
                || encoding == ContentEncoding::Auto);

        if can_encode {
            let body = match body {
                ResponseBody::Other(b) => match b {
                    Body::None => return ResponseBody::Other(Body::None),
                    Body::Empty => return ResponseBody::Other(Body::Empty),
                    Body::Bytes(buf) => EncoderBody::Bytes(buf),
                    Body::Message(stream) => EncoderBody::BoxedStream(stream),
                },
                ResponseBody::Body(stream) => EncoderBody::Stream(stream),
            };

            // Modify response body only if encoder is not None
            let encoder = ContentEncoder::encoder(encoding).unwrap();
            update_head(encoding, head);
            head.no_chunking(false);
            ResponseBody::Other(Body::from_message(Encoder {
                body,
                eof: false,
                fut: None,
                inner: Some(encoder),
            }))
        } else {
            body
        }
    }
}

impl Encoder<()> {
    /// Returns true if the encoder can produce `encoding`.
    pub(crate) fn can_encode(encoding: ContentEncoding) -> bool {
        ContentEncoder::can_encode(encoding)
    }
}

impl<B: fmt::Debug> fmt::Debug for Encoder<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Encoder")
            .field("eof", &self.eof)
            .field("body", &self.body)
            .field("encoder", &self.inner)
            .field("fut", &self.fut.as_ref().map(|_| "JoinHandle(_)"))
            .finish()
    }
}

enum EncoderBody<B> {
    Bytes(Bytes),
    Stream(B),
    BoxedStream(Box<dyn MessageBody>),
}

impl<B> fmt::Debug for EncoderBody<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EncoderBody::Bytes(b) => write!(f, "EncoderBody::Bytes({b:?})"),
            EncoderBody::Stream(_) => write!(f, "EncoderBody::Stream(_)"),
            EncoderBody::BoxedStream(_) => write!(f, "EncoderBody::BoxedStream(_)"),
        }
    }
}

impl<B: MessageBody> MessageBody for Encoder<B> {
    fn size(&self) -> BodySize {
        BodySize::Stream
    }

    fn poll_next_chunk(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, Rc<dyn std::error::Error>>>> {
        let result = self.poll_encoded(cx);
        if let Poll::Ready(Some(Err(_))) = result {
            // the encoder state is lost, the stream must not continue with raw data
            self.eof = true;
            self.inner = None;
            self.fut = None;
        }
        result
    }
}

impl<B: MessageBody> Encoder<B> {
    fn poll_encoded(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, Rc<dyn std::error::Error>>>> {
        loop {
            if self.eof {
                return Poll::Ready(None);
            }

            if let Some(ref mut fut) = self.fut {
                let mut encoder = match Pin::new(fut).poll(cx) {
                    Poll::Ready(Ok(Ok(item))) => item,
                    Poll::Ready(Ok(Err(e))) => return Poll::Ready(Some(Err(Rc::new(e)))),
                    Poll::Ready(Err(_)) => {
                        return Poll::Ready(Some(Err(Rc::new(io::Error::new(
                            io::ErrorKind::Interrupted,
                            "Canceled",
                        )))));
                    }
                    Poll::Pending => return Poll::Pending,
                };
                let chunk = encoder.take();
                self.inner = Some(encoder);
                self.fut.take();
                if !chunk.is_empty() {
                    return Poll::Ready(Some(Ok(chunk)));
                }
            }

            let result = match self.body {
                EncoderBody::Bytes(ref mut b) => {
                    if b.is_empty() {
                        Poll::Ready(None)
                    } else {
                        Poll::Ready(Some(Ok(std::mem::take(b))))
                    }
                }
                EncoderBody::Stream(ref mut b) => b.poll_next_chunk(cx),
                EncoderBody::BoxedStream(ref mut b) => b.poll_next_chunk(cx),
            };
            match result {
                Poll::Ready(Some(Ok(chunk))) => {
                    if let Some(mut encoder) = self.inner.take() {
                        if chunk.len() < INPLACE {
                            encoder.write(&chunk).map_err(dyn_rc_err)?;
                            let chunk = encoder.take();
                            self.inner = Some(encoder);
                            if !chunk.is_empty() {
                                return Poll::Ready(Some(Ok(chunk)));
                            }
                        } else {
                            self.fut = Some(spawn_blocking(move || {
                                encoder.write(&chunk)?;
                                Ok(encoder)
                            }));
                        }
                    } else {
                        return Poll::Ready(None);
                    }
                }
                Poll::Ready(None) => {
                    self.eof = true;
                    if let Some(encoder) = self.inner.take() {
                        let chunk = encoder.finish().map_err(dyn_rc_err)?;
                        if !chunk.is_empty() {
                            return Poll::Ready(Some(Ok(chunk)));
                        }
                    }
                    return Poll::Ready(None);
                }
                val => return val,
            }
        }
    }
}

fn update_head(encoding: ContentEncoding, head: &mut ResponseHead) {
    head.headers_mut().insert(
        CONTENT_ENCODING,
        HeaderValue::from_static(encoding.as_str()),
    );
}

enum ContentEncoder {
    Deflate(ZlibEncoder<Writer>),
    Gzip(GzEncoder<Writer>),
}

impl ContentEncoder {
    fn can_encode(encoding: ContentEncoding) -> bool {
        matches!(encoding, ContentEncoding::Deflate | ContentEncoding::Gzip)
    }

    fn encoder(encoding: ContentEncoding) -> Option<Self> {
        match encoding {
            ContentEncoding::Deflate => Some(ContentEncoder::Deflate(ZlibEncoder::new(
                Writer::new(),
                flate2::Compression::fast(),
            ))),
            ContentEncoding::Gzip => Some(ContentEncoder::Gzip(GzEncoder::new(
                Writer::new(),
                flate2::Compression::fast(),
            ))),
            _ => None,
        }
    }

    fn take(&mut self) -> Bytes {
        match *self {
            ContentEncoder::Deflate(ref mut encoder) => encoder.get_mut().take(),
            ContentEncoder::Gzip(ref mut encoder) => encoder.get_mut().take(),
        }
    }

    fn finish(self) -> Result<Bytes, io::Error> {
        match self {
            ContentEncoder::Gzip(encoder) => match encoder.finish() {
                Ok(writer) => Ok(writer.buf.freeze()),
                Err(err) => Err(err),
            },
            ContentEncoder::Deflate(encoder) => match encoder.finish() {
                Ok(writer) => Ok(writer.buf.freeze()),
                Err(err) => Err(err),
            },
        }
    }

    fn write(&mut self, data: &[u8]) -> Result<(), io::Error> {
        match *self {
            ContentEncoder::Gzip(ref mut encoder) => encoder
                .write_all(data)
                .inspect_err(|err| log::trace!("Failed to encode to gzip: {err}")),
            ContentEncoder::Deflate(ref mut encoder) => encoder
                .write_all(data)
                .inspect_err(|err| log::trace!("Failed to encode to deflate: {err}")),
        }
    }
}

impl fmt::Debug for ContentEncoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ContentEncoder::Deflate(_) => write!(f, "ContentEncoder::Deflate"),
            ContentEncoder::Gzip(_) => write!(f, "ContentEncoder::Gzip"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future::poll_fn;

    use super::*;

    #[crate::rt_test]
    async fn encoder_is_fused_after_error() {
        let mut enc = Encoder::<Body> {
            eof: false,
            body: EncoderBody::Bytes(Bytes::from_static(b"raw data")),
            inner: None,
            fut: Some(spawn_blocking(|| Err(io::Error::other("encode failed")))),
        };
        assert_eq!(enc.size(), BodySize::Stream);

        let res = poll_fn(|cx| enc.poll_next_chunk(cx)).await;
        assert!(matches!(res, Some(Err(_))));
        assert_eq!(enc.size(), BodySize::Stream);
        assert!(poll_fn(|cx| enc.poll_next_chunk(cx)).await.is_none());
    }

    struct EndOnce(bool);

    impl MessageBody for EndOnce {
        fn size(&self) -> BodySize {
            BodySize::Stream
        }

        fn poll_next_chunk(
            &mut self,
            _: &mut Context<'_>,
        ) -> Poll<Option<Result<Bytes, Rc<dyn std::error::Error>>>> {
            assert!(!self.0, "body polled after end of stream");
            self.0 = true;
            Poll::Ready(None)
        }
    }

    #[crate::rt_test]
    async fn encoder_is_fused_after_end_of_stream() {
        let mut enc = Encoder::<EndOnce> {
            eof: false,
            body: EncoderBody::Stream(EndOnce(false)),
            inner: None,
            fut: None,
        };
        assert!(poll_fn(|cx| enc.poll_next_chunk(cx)).await.is_none());
        assert!(poll_fn(|cx| enc.poll_next_chunk(cx)).await.is_none());
    }

    async fn collect(body: &mut ResponseBody<Body>) -> Vec<u8> {
        let mut buf = Vec::new();
        while let Some(chunk) = poll_fn(|cx| body.poll_next_chunk(cx)).await {
            buf.extend_from_slice(&chunk.unwrap());
        }
        buf
    }

    fn gunzip(data: &[u8]) -> Vec<u8> {
        use std::io::Read;

        let mut buf = Vec::new();
        flate2::read::GzDecoder::new(data)
            .read_to_end(&mut buf)
            .unwrap();
        buf
    }

    #[crate::rt_test]
    async fn encoder_response_bodies() {
        let mut head = ResponseHead::new(StatusCode::OK, crate::http::Version::HTTP_11);
        let body = Encoder::<Body>::response(ContentEncoding::Gzip, &mut head, Body::None.into());
        assert!(matches!(body, ResponseBody::Other(Body::None)));
        let body = Encoder::<Body>::response(ContentEncoding::Gzip, &mut head, Body::Empty.into());
        assert!(matches!(body, ResponseBody::Other(Body::Empty)));

        // identity encoding is not applied
        let body = Encoder::<Body>::response(
            ContentEncoding::Identity,
            &mut head,
            Body::from("data").into(),
        );
        assert!(matches!(body, ResponseBody::Other(Body::Bytes(_))));
        assert!(!head.headers().contains_key(CONTENT_ENCODING));

        // boxed message stream
        let mut head = ResponseHead::new(StatusCode::OK, crate::http::Version::HTTP_11);
        let inner = Body::from_message(Body::from("data"));
        let mut body = Encoder::<Body>::response(ContentEncoding::Gzip, &mut head, inner.into());
        assert_eq!(head.headers().get(CONTENT_ENCODING).unwrap(), "gzip");
        assert_eq!(gunzip(&collect(&mut body).await), b"data");

        // typed body stream
        let mut head = ResponseHead::new(StatusCode::OK, crate::http::Version::HTTP_11);
        let body = Encoder::<Body>::response(
            ContentEncoding::Gzip,
            &mut head,
            ResponseBody::Body(Body::from("typed")),
        );
        let ResponseBody::Other(Body::Message(mut msg)) = body else {
            panic!("expected encoded body")
        };
        let mut buf = Vec::new();
        while let Some(chunk) = poll_fn(|cx| msg.poll_next_chunk(cx)).await {
            buf.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(gunzip(&buf), b"typed");
    }

    #[crate::rt_test]
    async fn encoder_blocking_task_canceled() {
        let mut enc = Encoder::<Body> {
            eof: false,
            body: EncoderBody::Bytes(Bytes::new()),
            inner: None,
            fut: Some(spawn_blocking(|| panic!("encoder panic"))),
        };
        let res = poll_fn(|cx| enc.poll_next_chunk(cx)).await;
        assert!(matches!(res, Some(Err(ref e)) if e.to_string() == "Canceled"));
        assert!(poll_fn(|cx| enc.poll_next_chunk(cx)).await.is_none());
    }

    #[crate::rt_test]
    async fn encoder_without_content_encoder() {
        let mut enc = Encoder::<Body> {
            eof: false,
            body: EncoderBody::Bytes(Bytes::from_static(b"data")),
            inner: None,
            fut: None,
        };
        assert!(poll_fn(|cx| enc.poll_next_chunk(cx)).await.is_none());
    }

    #[test]
    fn encoder_debug() {
        assert!(ContentEncoder::encoder(ContentEncoding::Identity).is_none());
        for (encoding, name) in [
            (ContentEncoding::Gzip, "ContentEncoder::Gzip"),
            (ContentEncoding::Deflate, "ContentEncoder::Deflate"),
        ] {
            let enc = Encoder::<Body> {
                eof: false,
                body: EncoderBody::Bytes(Bytes::from_static(b"data")),
                inner: ContentEncoder::encoder(encoding),
                fut: None,
            };
            let s = format!("{enc:?}");
            assert!(s.contains(name) && s.contains("EncoderBody::Bytes"), "{s}");
        }

        let s = format!("{:?}", EncoderBody::Stream(Body::Empty));
        assert_eq!(s, "EncoderBody::Stream(_)");
        let s = format!(
            "{:?}",
            EncoderBody::<Body>::BoxedStream(Box::new(Body::Empty))
        );
        assert_eq!(s, "EncoderBody::BoxedStream(_)");
    }
}
