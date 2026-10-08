//! Stream encoder
use std::{
    collections::VecDeque, fmt, future::Future, io, pin::Pin, rc::Rc, task::Context, task::Poll,
};

use flate2::{Compress, Compression, Crc, FlushCompress, Status};
use zstd::zstd_safe::{CCtx, CParameter, InBuffer, OutBuffer, zstd_sys::ZSTD_EndDirective};

use crate::http::body::{Body, BodySize, MessageBody, ResponseBody};
use crate::http::header::{CONTENT_ENCODING, ContentEncoding, HeaderValue};
use crate::http::{ResponseHead, StatusCode};
use crate::rt::BlockingResult;
use crate::util::{BufMut, BytePageSize, Bytes, BytesMut, dyn_rc_err};

use super::{Spare, offload};

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
    /// The part of a large chunk that is not encoded yet
    pending: Bytes,
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
                pending: Bytes::new(),
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
            .field("pending", &self.pending.len())
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
            self.pending = Bytes::new();
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
            if let Some(chunk) = self.inner.as_mut().and_then(ContentEncoder::take) {
                return Poll::Ready(Some(Ok(chunk)));
            }

            if self.eof {
                self.inner = None;
                return Poll::Ready(None);
            }

            if let Some(ref mut fut) = self.fut {
                let encoder = match Pin::new(fut).poll(cx) {
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
                self.inner = Some(encoder);
                self.fut = None;
                continue;
            }

            if !self.pending.is_empty() {
                let chunk = std::mem::take(&mut self.pending);
                self.encode(chunk)?;
                continue;
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
                    if self.inner.is_none() {
                        return Poll::Ready(None);
                    }
                    self.encode(chunk)?;
                }
                Poll::Ready(None) => {
                    self.eof = true;
                    if let Some(encoder) = self.inner.as_mut() {
                        encoder.finish().map_err(dyn_rc_err)?;
                    }
                }
                val => return val,
            }
        }
    }
}

impl<B> Encoder<B> {
    /// Encodes a small chunk in place.
    ///
    /// A large chunk is encoded on the blocking thread pool, one part at a
    /// time, so the output of each part is sent before the next is encoded.
    fn encode(&mut self, mut chunk: Bytes) -> Result<(), Rc<dyn std::error::Error>> {
        let Some(mut encoder) = self.inner.take() else {
            return Ok(());
        };
        if chunk.len() < encoder.limit() {
            encoder.write(&chunk).map_err(dyn_rc_err)?;
            self.inner = Some(encoder);
        } else {
            let part = chunk.split_to(chunk.len().min(encoder.task_size()));
            self.pending = chunk;
            self.fut = Some(offload(move || {
                encoder.write(&part)?;
                Ok(encoder)
            }));
        }
        Ok(())
    }
}

fn update_head(encoding: ContentEncoding, head: &mut ResponseHead) {
    head.headers_mut().insert(
        CONTENT_ENCODING,
        HeaderValue::from_static(encoding.as_str()),
    );
}

/// The `gzip` header written by the encoder, without a name or modification
/// time. The extra flags byte marks the fastest compression level.
const GZIP_HEADER: [u8; 10] = [0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 4, 255];

/// Compresses into pages of a buffer, full pages are queued for sending.
struct ContentEncoder {
    codec: Codec,
    buf: BytesMut,
    chunks: VecDeque<Bytes>,
}

enum Codec {
    Deflate(Compress),
    /// The checksum and size of the input for the trailer
    Gzip(Compress, Crc),
    Zstd(CCtx<'static>),
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
        match self.codec {
            Codec::Deflate(_) | Codec::Gzip(..) => 16 * 1024,
            Codec::Zstd(_) => 512 * 1024,
        }
    }

    /// The most input a single blocking task encodes.
    const fn task_size(&self) -> usize {
        match self.codec {
            Codec::Deflate(_) | Codec::Gzip(..) => 256 * 1024,
            Codec::Zstd(_) => 1024 * 1024,
        }
    }

    /// Creates an encoder, `size` is the length of the body if it is known.
    fn encoder(encoding: ContentEncoding, size: Option<u64>) -> Option<Self> {
        let mut buf = BytesMut::with_page_size(BytePageSize::Size32);
        let codec = match encoding {
            ContentEncoding::Deflate => Codec::Deflate(Compress::new(Compression::fast(), true)),
            ContentEncoding::Gzip => {
                buf.extend_from_slice(&GZIP_HEADER);
                Codec::Gzip(Compress::new(Compression::fast(), false), Crc::new())
            }
            // with a known size zstd picks a smaller window and stores the size in the frame
            ContentEncoding::Zstd => {
                let mut ctx = CCtx::try_create()?;
                ctx.set_parameter(CParameter::CompressionLevel(0)).ok()?;
                ctx.set_pledged_src_size(size).ok()?;
                Codec::Zstd(ctx)
            }
            _ => return None,
        };
        Some(ContentEncoder {
            codec,
            buf,
            chunks: VecDeque::new(),
        })
    }

    /// Returns the encoded output, a full page first.
    fn take(&mut self) -> Option<Bytes> {
        self.chunks
            .pop_front()
            .or_else(|| (!self.buf.is_empty()).then(|| self.buf.take()))
    }

    /// Queues a full page and starts a new one.
    ///
    /// A page is never grown, so its output is not copied.
    fn reserve(&mut self) {
        if self.buf.remaining_mut() == 0 {
            if !self.buf.is_empty() {
                self.chunks.push_back(self.buf.take());
            }
            self.buf.reserve_more();
        }
    }

    /// Compresses `data` with `flush` into the buffer, returns `true` at the end of the stream.
    fn compress(&mut self, data: &mut &[u8], flush: FlushCompress) -> io::Result<bool> {
        self.reserve();
        let (Codec::Deflate(inner) | Codec::Gzip(inner, _)) = &mut self.codec else {
            unreachable!()
        };
        let (total_in, total_out) = (inner.total_in(), inner.total_out());
        // SAFETY: the encoder only writes to the slice, and `advance_mut`
        // covers just the bytes it has written
        let status = unsafe {
            let spare = self.buf.chunk_mut().as_uninit_slice_mut();
            inner
                .compress_uninit(data, spare, flush)
                .map_err(io::Error::other)?
        };
        let read = usize::try_from(inner.total_in() - total_in).unwrap();
        let written = usize::try_from(inner.total_out() - total_out).unwrap();
        unsafe { self.buf.advance_mut(written) };
        *data = &data[read..];

        if status == Status::StreamEnd {
            Ok(true)
        } else if read == 0 && written == 0 {
            Err(io::ErrorKind::WriteZero.into())
        } else {
            Ok(false)
        }
    }

    /// Compresses `data` with `end` into the buffer, returns the size of the
    /// output the `zstd` context still holds at the end of the frame.
    fn compress_zstd(&mut self, data: &mut &[u8], end: ZSTD_EndDirective) -> io::Result<usize> {
        self.reserve();
        let Codec::Zstd(ctx) = &mut self.codec else {
            unreachable!()
        };
        let mut src = InBuffer::around(data);
        let mut spare = Spare::new(&mut self.buf);
        let mut dst = OutBuffer::around(&mut spare);
        let left = ctx
            .compress_stream2(&mut dst, &mut src, end)
            .map_err(zstd_error)?;
        let read = src.pos();
        *data = &data[read..];
        Ok(left)
    }

    /// Writes the end of the stream.
    fn finish(&mut self) -> io::Result<()> {
        if let Codec::Zstd(_) = self.codec {
            while self.compress_zstd(&mut &[][..], ZSTD_EndDirective::ZSTD_e_end)? != 0 {}
            return Ok(());
        }

        while !self.compress(&mut &[][..], FlushCompress::Finish)? {}
        if let Codec::Gzip(_, crc) = &self.codec {
            let mut trailer = [0; 8];
            trailer[..4].copy_from_slice(&crc.sum().to_le_bytes());
            trailer[4..].copy_from_slice(&crc.amount().to_le_bytes());
            let mut trailer = &trailer[..];
            while !trailer.is_empty() {
                self.reserve();
                let size = trailer.len().min(self.buf.remaining_mut());
                self.buf.extend_from_slice(&trailer[..size]);
                trailer = &trailer[size..];
            }
        }
        Ok(())
    }

    fn write(&mut self, mut data: &[u8]) -> Result<(), io::Error> {
        if let Codec::Gzip(_, crc) = &mut self.codec {
            crc.update(data);
        }
        while !data.is_empty() {
            if let Codec::Zstd(_) = self.codec {
                self.compress_zstd(&mut data, ZSTD_EndDirective::ZSTD_e_continue)
                    .inspect_err(|err| log::trace!("Failed to encode to zstd: {err}"))?;
            } else {
                self.compress(&mut data, FlushCompress::None)
                    .inspect_err(|err| log::trace!("Failed to encode to {self:?}: {err}"))?;
            }
        }
        Ok(())
    }
}

fn zstd_error(code: usize) -> io::Error {
    io::Error::other(zstd::zstd_safe::get_error_name(code))
}

impl fmt::Debug for ContentEncoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.codec {
            Codec::Deflate(_) => write!(f, "ContentEncoder::Deflate"),
            Codec::Gzip(..) => write!(f, "ContentEncoder::Gzip"),
            Codec::Zstd(_) => write!(f, "ContentEncoder::Zstd"),
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
            pending: Bytes::new(),
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
            pending: Bytes::new(),
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

    /// Incompressible data, so the encoded size is close to `len`.
    fn random(len: usize) -> Vec<u8> {
        let mut x = 0x2545_f491_4f6c_dd1d_u64;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    #[crate::rt_test]
    async fn encoder_splits_large_chunks() {
        for (encoding, task, limit) in [
            (ContentEncoding::Gzip, 256 * 1024, 16 * 1024),
            (ContentEncoding::Deflate, 256 * 1024, 16 * 1024),
            (ContentEncoding::Zstd, 1024 * 1024, 512 * 1024),
        ] {
            // two parts on the pool, the rest is below the limit and encoded in place
            let data = random(2 * task + limit / 2);
            let before = super::super::offloaded();
            let mut head = ResponseHead::new(StatusCode::OK, crate::http::Version::HTTP_11);
            let mut body =
                Encoder::<Body>::response(encoding, &mut head, Body::from(data.clone()).into());

            let mut encoded = Vec::new();
            let mut chunks = 0;
            while let Some(chunk) = poll_fn(|cx| body.poll_next_chunk(cx)).await {
                let chunk = chunk.unwrap();
                assert!(chunk.len() <= 32 * 1024, "{encoding:?} {}", chunk.len());
                encoded.extend_from_slice(&chunk);
                chunks += 1;
            }
            assert!(
                chunks > encoded.len() / (32 * 1024),
                "{encoding:?} {chunks}"
            );
            assert_eq!(decompress(encoding, &encoded), data);
            assert_eq!(super::super::offloaded() - before, 2, "{encoding:?}");
        }
    }

    #[crate::rt_test]
    async fn encoder_drops_pending_on_error() {
        let mut enc = Encoder::<Body> {
            eof: false,
            pending: Bytes::from_static(b"rest"),
            body: EncoderBody::Bytes(Bytes::new()),
            inner: None,
            fut: Some(spawn_blocking(|| Err(io::Error::other("encode failed")))),
        };
        assert!(matches!(
            poll_fn(|cx| enc.poll_next_chunk(cx)).await,
            Some(Err(_))
        ));
        assert!(enc.pending.is_empty());
        assert!(poll_fn(|cx| enc.poll_next_chunk(cx)).await.is_none());
    }

    #[crate::rt_test]
    async fn encoder_blocking_task_canceled() {
        let mut enc = Encoder::<Body> {
            eof: false,
            pending: Bytes::new(),
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
            pending: Bytes::new(),
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
                pending: Bytes::new(),
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

    fn encode_all(encoding: ContentEncoding, data: &[u8]) -> Vec<Bytes> {
        let mut enc = ContentEncoder::encoder(encoding, None).unwrap();
        enc.write(data).unwrap();
        enc.finish().unwrap();
        std::iter::from_fn(|| enc.take()).collect()
    }

    #[test]
    fn encoder_writes_pages() {
        // the output of a large write is queued page by page
        for encoding in [
            ContentEncoding::Gzip,
            ContentEncoding::Deflate,
            ContentEncoding::Zstd,
        ] {
            let data = random(200 * 1024);
            let chunks = encode_all(encoding, &data);
            assert!(chunks.len() > 6, "{encoding:?} {}", chunks.len());
            assert!(chunks.iter().all(|c| c.len() <= 32 * 1024 && !c.is_empty()));
            assert_eq!(decompress(encoding, &chunks.concat()), data);
        }

        // gzip has the same header as flate2
        let chunks = encode_all(ContentEncoding::Gzip, b"data");
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut e, b"data").unwrap();
        assert_eq!(chunks[0][..10], e.finish().unwrap()[..10]);
    }

    #[test]
    fn encoder_finish_across_pages() {
        // some sizes end the compressed data a few bytes before the end of a
        // page, the end of the stream continues on the next one
        let data = random(33 * 1024);
        for encoding in [
            ContentEncoding::Gzip,
            ContentEncoding::Deflate,
            ContentEncoding::Zstd,
        ] {
            let mut split = 0;
            for len in 32 * 1024 - 64..32 * 1024 {
                let chunks = encode_all(encoding, &data[..len]);
                assert!(chunks.iter().all(|c| c.len() <= 32 * 1024));
                if chunks.len() > 1 && chunks.last().unwrap().len() < 8 {
                    split += 1;
                }
                assert_eq!(decompress(encoding, &chunks.concat()), &data[..len]);
            }
            assert!(split > 0, "{encoding:?}");
        }
    }

    #[test]
    fn encoder_write_after_full_page() {
        // a write can leave a full page, `zstd` does with these sizes, the
        // next write starts a new page and does not queue an empty chunk
        let mut full = 0;
        for encoding in [
            ContentEncoding::Gzip,
            ContentEncoding::Deflate,
            ContentEncoding::Zstd,
        ] {
            let data = random(256 * 1024);
            let mut enc = ContentEncoder::encoder(encoding, None).unwrap();
            let mut out = Vec::new();
            for part in data.chunks(16 * 1024) {
                enc.write(part).unwrap();
                full += usize::from(enc.buf.remaining_mut() == 0);
                while let Some(chunk) = enc.take() {
                    assert!(!chunk.is_empty(), "{encoding:?}");
                    out.extend_from_slice(&chunk);
                }
            }
            enc.finish().unwrap();
            while let Some(chunk) = enc.take() {
                assert!(!chunk.is_empty(), "{encoding:?}");
                out.extend_from_slice(&chunk);
            }
            assert_eq!(decompress(encoding, &out), data);
        }
        assert!(full > 0);
    }
}
