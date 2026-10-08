//! Stream encoder
use std::{fmt, future::Future, io, io::Write, pin::Pin, rc::Rc, task::Context, task::Poll};

use flate2::write::{GzEncoder, ZlibEncoder};
use zstd::stream::write::Encoder as ZstdEncoder;

use crate::http::body::{Body, BodySize, MessageBody, ResponseBody};
use crate::http::header::{CONTENT_ENCODING, ContentEncoding, HeaderValue};
use crate::http::{ResponseHead, StatusCode};
use crate::rt::BlockingResult;
use crate::util::{Bytes, dyn_rc_err};

use super::{Writer, offload};

/// Bodies of a known size below this are sent without compression.
///
/// Small bodies barely shrink, while every encoder allocates its state.
const MIN_SIZE: u64 = 1024;

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
    /// `101 Switching Protocols` or `204 No Content`, or if the body is empty
    /// or its size is known to be below 1KiB.
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
            let size = match body.size() {
                BodySize::None | BodySize::Empty => return body,
                BodySize::Sized(size) if size < MIN_SIZE => return body,
                BodySize::Sized(size) => Some(size),
                BodySize::Stream => None,
            };
            let Some(encoder) = ContentEncoder::encoder(encoding, size) else {
                return body;
            };
            let body = match body {
                ResponseBody::Other(b) => match b {
                    Body::None | Body::Empty => unreachable!(),
                    Body::Bytes(buf) => EncoderBody::Bytes(buf),
                    Body::Message(stream) => EncoderBody::BoxedStream(stream),
                },
                ResponseBody::Body(stream) => EncoderBody::Stream(stream),
            };

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
                        if chunk.len() < encoder.limit() {
                            encoder.write(&chunk).map_err(dyn_rc_err)?;
                            let chunk = encoder.take();
                            self.inner = Some(encoder);
                            if !chunk.is_empty() {
                                return Poll::Ready(Some(Ok(chunk)));
                            }
                        } else {
                            self.fut = Some(offload(move || {
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
    Zstd(ZstdEncoder<'static, Writer>),
}

impl ContentEncoder {
    fn can_encode(encoding: ContentEncoding) -> bool {
        matches!(
            encoding,
            ContentEncoding::Deflate | ContentEncoding::Gzip | ContentEncoding::Zstd
        )
    }

    /// Chunks of this size and larger are encoded on the blocking thread pool.
    ///
    /// `gzip` and `deflate` are several times slower than `zstd`, so they are
    /// offloaded much earlier.
    const fn limit(&self) -> usize {
        match self {
            ContentEncoder::Deflate(_) | ContentEncoder::Gzip(_) => 16 * 1024,
            ContentEncoder::Zstd(_) => 512 * 1024,
        }
    }

    /// Creates an encoder, `size` is the length of the body if it is known.
    fn encoder(encoding: ContentEncoding, size: Option<u64>) -> Option<Self> {
        match encoding {
            ContentEncoding::Deflate => Some(ContentEncoder::Deflate(ZlibEncoder::new(
                Writer::new(),
                flate2::Compression::fast(),
            ))),
            ContentEncoding::Gzip => Some(ContentEncoder::Gzip(GzEncoder::new(
                Writer::new(),
                flate2::Compression::fast(),
            ))),
            // with a known size zstd picks a smaller window and stores the size in the frame
            ContentEncoding::Zstd => ZstdEncoder::new(Writer::new(), 0)
                .and_then(|mut encoder| {
                    encoder.set_pledged_src_size(size)?;
                    Ok(encoder)
                })
                .ok()
                .map(ContentEncoder::Zstd),
            _ => None,
        }
    }

    fn take(&mut self) -> Bytes {
        match *self {
            ContentEncoder::Deflate(ref mut encoder) => encoder.get_mut().take(),
            ContentEncoder::Gzip(ref mut encoder) => encoder.get_mut().take(),
            ContentEncoder::Zstd(ref mut encoder) => encoder.get_mut().take(),
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
            ContentEncoder::Zstd(encoder) => match encoder.finish() {
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
            ContentEncoder::Zstd(ref mut encoder) => encoder
                .write_all(data)
                .inspect_err(|err| log::trace!("Failed to encode to zstd: {err}")),
        }
    }
}

impl fmt::Debug for ContentEncoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ContentEncoder::Deflate(_) => write!(f, "ContentEncoder::Deflate"),
            ContentEncoder::Gzip(_) => write!(f, "ContentEncoder::Gzip"),
            ContentEncoder::Zstd(_) => write!(f, "ContentEncoder::Zstd"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future::poll_fn;

    use super::*;
    use crate::rt::spawn_blocking;

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
        let data = "data".repeat(256);
        let mut head = ResponseHead::new(StatusCode::OK, crate::http::Version::HTTP_11);
        let inner = Body::from_message(Body::from(data.clone()));
        let mut body = Encoder::<Body>::response(ContentEncoding::Gzip, &mut head, inner.into());
        assert_eq!(head.headers().get(CONTENT_ENCODING).unwrap(), "gzip");
        assert_eq!(gunzip(&collect(&mut body).await), data.as_bytes());

        // typed body stream
        let typed = "typed".repeat(205);
        let mut head = ResponseHead::new(StatusCode::OK, crate::http::Version::HTTP_11);
        let body = Encoder::<Body>::response(
            ContentEncoding::Gzip,
            &mut head,
            ResponseBody::Body(Body::from(typed.clone())),
        );
        let ResponseBody::Other(Body::Message(mut msg)) = body else {
            panic!("expected encoded body")
        };
        let mut buf = Vec::new();
        while let Some(chunk) = poll_fn(|cx| msg.poll_next_chunk(cx)).await {
            buf.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(gunzip(&buf), typed.as_bytes());
    }

    struct Chunks(Vec<Bytes>, BodySize);

    impl MessageBody for Chunks {
        fn size(&self) -> BodySize {
            self.1
        }

        fn poll_next_chunk(
            &mut self,
            _: &mut Context<'_>,
        ) -> Poll<Option<Result<Bytes, Rc<dyn std::error::Error>>>> {
            Poll::Ready(self.0.pop().map(Ok))
        }
    }

    #[crate::rt_test]
    async fn encoder_skips_small_bodies() {
        for encoding in [
            ContentEncoding::Gzip,
            ContentEncoding::Deflate,
            ContentEncoding::Zstd,
        ] {
            let small = Bytes::from(vec![b'x'; 1023]);
            let large = Bytes::from(vec![b'x'; 1024]);

            let mut head = ResponseHead::new(StatusCode::OK, crate::http::Version::HTTP_11);
            let body =
                Encoder::<Body>::response(encoding, &mut head, Body::from(small.clone()).into());
            assert!(matches!(body, ResponseBody::Other(Body::Bytes(ref b)) if *b == small));
            assert!(!head.headers().contains_key(CONTENT_ENCODING));

            let mut head = ResponseHead::new(StatusCode::OK, crate::http::Version::HTTP_11);
            let stream = Chunks(vec![small.clone()], BodySize::Sized(1023));
            let body = Encoder::response(encoding, &mut head, ResponseBody::Body(stream));
            assert!(matches!(body, ResponseBody::Body(_)));
            assert!(!head.headers().contains_key(CONTENT_ENCODING));

            let mut head = ResponseHead::new(StatusCode::OK, crate::http::Version::HTTP_11);
            let mut body =
                Encoder::<Body>::response(encoding, &mut head, Body::from(large.clone()).into());
            assert_eq!(
                head.headers().get(CONTENT_ENCODING).unwrap(),
                encoding.as_str()
            );
            assert_eq!(decompress(encoding, &collect(&mut body).await), large);

            // a stream of unknown size is encoded
            let mut head = ResponseHead::new(StatusCode::OK, crate::http::Version::HTTP_11);
            let stream = Chunks(vec![Bytes::from_static(b"x")], BodySize::Stream);
            let mut body =
                Encoder::<Body>::response(encoding, &mut head, Body::from_message(stream).into());
            assert_eq!(
                head.headers().get(CONTENT_ENCODING).unwrap(),
                encoding.as_str()
            );
            assert_eq!(decompress(encoding, &collect(&mut body).await), b"x");
        }
    }

    fn zstd_content_size(data: &[u8]) -> Option<u64> {
        zstd::zstd_safe::get_frame_content_size(data).unwrap()
    }

    #[crate::rt_test]
    async fn encoder_zstd_known_size() {
        let data = Bytes::from(vec![b'x'; 4096]);

        // the frame stores the size of a sized body
        let mut head = ResponseHead::new(StatusCode::OK, crate::http::Version::HTTP_11);
        let mut body = Encoder::<Body>::response(
            ContentEncoding::Zstd,
            &mut head,
            Body::from(data.clone()).into(),
        );
        let frame = collect(&mut body).await;
        assert_eq!(zstd_content_size(&frame), Some(4096));
        assert_eq!(decompress(ContentEncoding::Zstd, &frame), data);

        let mut head = ResponseHead::new(StatusCode::OK, crate::http::Version::HTTP_11);
        let chunks = vec![data.slice(2048..), data.slice(..2048)];
        let stream = Body::from_message(Chunks(chunks, BodySize::Sized(4096)));
        let mut body = Encoder::<Body>::response(ContentEncoding::Zstd, &mut head, stream.into());
        let frame = collect(&mut body).await;
        assert_eq!(zstd_content_size(&frame), Some(4096));
        assert_eq!(decompress(ContentEncoding::Zstd, &frame), data);

        // the size of a stream is unknown
        let mut head = ResponseHead::new(StatusCode::OK, crate::http::Version::HTTP_11);
        let stream = Body::from_message(Chunks(vec![data.clone()], BodySize::Stream));
        let mut body = Encoder::<Body>::response(ContentEncoding::Zstd, &mut head, stream.into());
        let frame = collect(&mut body).await;
        assert_eq!(zstd_content_size(&frame), None);
        assert_eq!(decompress(ContentEncoding::Zstd, &frame), data);

        // a body larger than its size fails
        let mut head = ResponseHead::new(StatusCode::OK, crate::http::Version::HTTP_11);
        let stream = Body::from_message(Chunks(vec![data.clone()], BodySize::Sized(2048)));
        let mut body = Encoder::<Body>::response(ContentEncoding::Zstd, &mut head, stream.into());
        assert!(matches!(
            poll_fn(|cx| body.poll_next_chunk(cx)).await,
            Some(Err(_))
        ));
    }

    fn decompress(encoding: ContentEncoding, data: &[u8]) -> Vec<u8> {
        use std::io::Read;

        let mut buf = Vec::new();
        match encoding {
            ContentEncoding::Gzip => return gunzip(data),
            ContentEncoding::Deflate => {
                flate2::read::ZlibDecoder::new(data)
                    .read_to_end(&mut buf)
                    .unwrap();
            }
            ContentEncoding::Zstd => buf = zstd::decode_all(data).unwrap(),
            _ => unreachable!(),
        }
        buf
    }

    #[crate::rt_test]
    async fn encoder_offloads_large_chunks() {
        for (encoding, limit) in [
            (ContentEncoding::Gzip, 16 * 1024),
            (ContentEncoding::Deflate, 16 * 1024),
            (ContentEncoding::Zstd, 512 * 1024),
        ] {
            for (len, offloaded) in [(limit - 1, 0), (limit, 1)] {
                let data: Vec<u8> = (0..len).map(|i: usize| (i % 251) as u8).collect();
                let before = super::super::offloaded();
                let mut head = ResponseHead::new(StatusCode::OK, crate::http::Version::HTTP_11);
                let mut body =
                    Encoder::<Body>::response(encoding, &mut head, Body::from(data.clone()).into());
                assert_eq!(
                    head.headers().get(CONTENT_ENCODING).unwrap(),
                    encoding.as_str()
                );
                assert_eq!(decompress(encoding, &collect(&mut body).await), data);
                assert_eq!(
                    super::super::offloaded() - before,
                    offloaded,
                    "{encoding:?} {len}"
                );
            }
        }
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
        assert!(ContentEncoder::encoder(ContentEncoding::Identity, None).is_none());
        for (encoding, name) in [
            (ContentEncoding::Gzip, "ContentEncoder::Gzip"),
            (ContentEncoding::Deflate, "ContentEncoder::Deflate"),
            (ContentEncoding::Zstd, "ContentEncoder::Zstd"),
        ] {
            let enc = Encoder::<Body> {
                eof: false,
                body: EncoderBody::Bytes(Bytes::from_static(b"data")),
                inner: ContentEncoder::encoder(encoding, None),
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
