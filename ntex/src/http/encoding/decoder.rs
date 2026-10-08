use std::{future::Future, io, io::Write, pin::Pin, task::Context, task::Poll};

use flate2::write::{GzDecoder, ZlibDecoder};
use zstd::stream::raw::{self, DParameter, InBuffer, Operation, OutBuffer};
use zstd::zstd_safe::WriteBuf;

use super::{Writer, offload};
use crate::http::error::PayloadError;
use crate::http::header::{CONTENT_ENCODING, ContentEncoding, HeaderMap};
use crate::rt::BlockingResult;
use crate::util::{BufMut, BytePageSize, Bytes, BytesMut, Stream};

/// Decoding stops at this output size, the rest of the input is decoded by the next poll.
///
/// A single write or flush of a `gzip` or `deflate` decoder adds at most 32KiB,
/// so their chunks stay below 96KiB. `zstd` chunks never exceed this size.
const MAX_CHUNK_SIZE: usize = 32 * 1024;

/// The largest `zstd` window a decoder accepts, 8MiB as required by RFC 9659.
const ZSTD_WINDOW_LOG_MAX: u32 = 23;

/// Payload stream decoder.
///
/// Decompresses a stream of payload chunks. `gzip`, `deflate` and `zstd` are
/// decoded; other encodings pass the stream through unchanged.
#[derive(derive_more::Debug)]
pub struct Decoder<S> {
    #[debug(skip)]
    inner: Option<ContentDecoder>,
    stream: S,
    eof: bool,
    /// The stream is decoded
    decode: bool,
    /// Input that is not decoded yet
    #[debug(skip)]
    pending: Option<Bytes>,
    #[debug(skip)]
    fut: Option<BlockingResult<DecodeResult>>,
}

type DecodeResult = Result<(Option<Bytes>, ContentDecoder, Bytes), io::Error>;

impl<S> Decoder<S>
where
    S: Stream<Item = Result<Bytes, PayloadError>>,
{
    /// Construct a decoder for the given content encoding.
    #[inline]
    pub fn new(stream: S, encoding: ContentEncoding) -> Decoder<S> {
        let inner = match encoding {
            ContentEncoding::Deflate => Some(ContentDecoder::Deflate(Box::new(ZlibDecoder::new(
                Writer::new(),
            )))),
            ContentEncoding::Gzip => Some(ContentDecoder::Gzip(Box::new(GzDecoder::new(
                Writer::new(),
            )))),
            ContentEncoding::Zstd => ZstdDecoder::new()
                .inspect_err(|err| log::error!("Cannot create zstd decoder: {err}"))
                .ok()
                .map(|d| ContentDecoder::Zstd(Box::new(d))),
            _ => None,
        };
        Decoder {
            decode: inner.is_some(),
            inner,
            stream,
            fut: None,
            eof: false,
            pending: None,
        }
    }

    /// Returns `true` if the stream is decoded.
    pub(crate) fn is_decoding(&self) -> bool {
        self.inner.is_some()
    }

    /// Construct decoder based on the `Content-Encoding` header.
    ///
    /// A missing or invalid header selects the `Identity` encoding.
    #[inline]
    pub fn from_headers(stream: S, headers: &HeaderMap) -> Decoder<S> {
        // check content-encoding
        let encoding = if let Some(enc) = headers.get(&CONTENT_ENCODING) {
            if let Ok(enc) = enc.to_str() {
                ContentEncoding::from(enc)
            } else {
                ContentEncoding::Identity
            }
        } else {
            ContentEncoding::Identity
        };

        Self::new(stream, encoding)
    }
}

impl<S> Stream for Decoder<S>
where
    S: Stream<Item = Result<Bytes, PayloadError>> + Unpin,
{
    type Item = Result<Bytes, PayloadError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let result = self.poll_decoded(cx);
        if let Poll::Ready(Some(Err(_))) = result
            && self.decode
            && self.inner.is_none()
        {
            // the decoder state is lost, the stream must not continue with raw data
            self.eof = true;
            self.fut = None;
            self.pending = None;
        }
        result
    }
}

impl<S> Decoder<S>
where
    S: Stream<Item = Result<Bytes, PayloadError>> + Unpin,
{
    /// Puts the decoder back after a feed, with the input it left.
    fn restore(&mut self, decoder: ContentDecoder, rest: Bytes) {
        if !rest.is_empty() || decoder.has_more() {
            self.pending = Some(rest);
        }
        self.inner = Some(decoder);
    }

    fn poll_decoded(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<Bytes, PayloadError>>> {
        loop {
            if let Some(ref mut fut) = self.fut {
                let (chunk, decoder, rest) = match Pin::new(fut).poll(cx) {
                    Poll::Ready(Ok(Ok(item))) => item,
                    Poll::Ready(Ok(Err(e))) => return Poll::Ready(Some(Err(e.into()))),
                    Poll::Ready(Err(e)) => return Poll::Ready(Some(Err(e.into()))),
                    Poll::Pending => return Poll::Pending,
                };
                self.fut = None;
                self.restore(decoder, rest);
                if let Some(chunk) = chunk {
                    return Poll::Ready(Some(Ok(chunk)));
                }
            }

            if let Some(mut data) = self.pending.take() {
                let mut decoder = self.inner.take().unwrap();
                if data.len() < decoder.limit() {
                    let chunk = decoder.feed_data(&mut data)?;
                    self.restore(decoder, data);
                    if let Some(chunk) = chunk {
                        return Poll::Ready(Some(Ok(chunk)));
                    }
                } else {
                    self.fut = Some(offload(move || {
                        let chunk = decoder.feed_data(&mut data)?;
                        Ok((chunk, decoder, data))
                    }));
                }
                continue;
            }

            if self.eof {
                return Poll::Ready(None);
            }

            match Pin::new(&mut self.stream).poll_next(cx) {
                Poll::Ready(Some(Err(err))) => return Poll::Ready(Some(Err(err))),
                Poll::Ready(Some(Ok(chunk))) => {
                    if self.inner.is_some() {
                        if !chunk.is_empty() {
                            self.pending = Some(chunk);
                        }
                        continue;
                    }
                    return Poll::Ready(Some(Ok(chunk)));
                }
                Poll::Ready(None) => {
                    self.eof = true;
                    return if let Some(mut decoder) = self.inner.take() {
                        match decoder.feed_eof() {
                            Ok(Some(res)) => Poll::Ready(Some(Ok(res))),
                            Ok(None) => Poll::Ready(None),
                            Err(err) => Poll::Ready(Some(Err(err.into()))),
                        }
                    } else {
                        Poll::Ready(None)
                    };
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

enum ContentDecoder {
    Deflate(Box<ZlibDecoder<Writer>>),
    Gzip(Box<GzDecoder<Writer>>),
    Zstd(Box<ZstdDecoder>),
}

impl ContentDecoder {
    /// Input of this size and larger is decoded on the blocking thread pool.
    ///
    /// The work of a single feed is bounded by `MAX_CHUNK_SIZE` anyway, the
    /// pool only takes it off the current thread.
    const fn limit(&self) -> usize {
        match self {
            ContentDecoder::Deflate(_) | ContentDecoder::Gzip(_) => 128 * 1024,
            ContentDecoder::Zstd(_) => 512 * 1024,
        }
    }

    /// Returns `true` if decoded output is left without new input.
    fn has_more(&self) -> bool {
        match self {
            ContentDecoder::Deflate(_) | ContentDecoder::Gzip(_) => false,
            ContentDecoder::Zstd(decoder) => decoder.more,
        }
    }

    fn feed_eof(&mut self) -> io::Result<Option<Bytes>> {
        match self {
            ContentDecoder::Zstd(decoder) => decoder.finish().map(|()| None),
            ContentDecoder::Gzip(decoder) => match decoder.try_finish() {
                Ok(()) => {
                    let b = decoder.get_mut().take();
                    if b.is_empty() { Ok(None) } else { Ok(Some(b)) }
                }
                Err(e) => Err(e),
            },
            ContentDecoder::Deflate(decoder) => match decoder.try_finish() {
                Ok(()) => {
                    let b = decoder.get_mut().take();
                    if b.is_empty() { Ok(None) } else { Ok(Some(b)) }
                }
                Err(e) => Err(e),
            },
        }
    }

    /// Decodes `data` until the output reaches `MAX_CHUNK_SIZE`.
    ///
    /// Decoded input is removed from `data`.
    fn feed_data(&mut self, data: &mut Bytes) -> io::Result<Option<Bytes>> {
        if let ContentDecoder::Zstd(decoder) = self {
            return decoder.feed(data);
        }

        while !data.is_empty() && self.output_len() < MAX_CHUNK_SIZE {
            let n = match self {
                ContentDecoder::Gzip(decoder) => decoder.write(data)?,
                ContentDecoder::Deflate(decoder) => decoder.write(data)?,
                ContentDecoder::Zstd(_) => unreachable!(),
            };
            if n == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            data.advance_to(n);
        }
        if data.is_empty() {
            match self {
                ContentDecoder::Gzip(decoder) => decoder.flush()?,
                ContentDecoder::Deflate(decoder) => decoder.flush()?,
                ContentDecoder::Zstd(_) => unreachable!(),
            }
        }

        let b = match self {
            ContentDecoder::Gzip(decoder) => decoder.get_mut().take(),
            ContentDecoder::Deflate(decoder) => decoder.get_mut().take(),
            ContentDecoder::Zstd(_) => unreachable!(),
        };
        if b.is_empty() { Ok(None) } else { Ok(Some(b)) }
    }

    fn output_len(&self) -> usize {
        match self {
            ContentDecoder::Gzip(decoder) => decoder.get_ref().len(),
            ContentDecoder::Deflate(decoder) => decoder.get_ref().len(),
            ContentDecoder::Zstd(_) => 0,
        }
    }
}

struct ZstdDecoder {
    decoder: raw::Decoder<'static>,
    buf: BytesMut,
    /// The last frame is complete
    done: bool,
    /// The output buffer was filled, the decoder may hold more output
    more: bool,
}

impl ZstdDecoder {
    fn new() -> io::Result<Self> {
        let mut decoder = raw::Decoder::new()?;
        decoder.set_parameter(DParameter::WindowLogMax(ZSTD_WINDOW_LOG_MAX))?;
        Ok(ZstdDecoder {
            decoder,
            buf: BytesMut::with_page_size(BytePageSize::Size32),
            done: false,
            more: false,
        })
    }

    /// Decodes `data` until the output buffer is full.
    fn feed(&mut self, data: &mut Bytes) -> io::Result<Option<Bytes>> {
        self.buf.reserve_more();
        while self.buf.remaining_mut() > 0 && (!data.is_empty() || self.more) {
            let mut src = InBuffer::around(data);
            let mut spare = Spare::new(&mut self.buf);
            let mut dst = OutBuffer::around(&mut spare);
            // a new frame starts automatically after the previous one is complete
            let hint = self.decoder.run(&mut src, &mut dst)?;
            let (read, written) = (src.pos(), dst.pos());
            self.more = self.buf.remaining_mut() == 0;
            if read == 0 && written == 0 {
                break;
            }
            self.done = hint == 0;
            data.advance_to(read);
        }
        if self.buf.is_empty() {
            Ok(None)
        } else {
            Ok(Some(self.buf.take()))
        }
    }

    fn finish(&self) -> io::Result<()> {
        if self.done {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "zstd stream is incomplete",
            ))
        }
    }
}

/// The spare capacity of a buffer, `zstd` writes decoded data directly into it.
struct Spare<'a> {
    buf: &'a mut BytesMut,
    start: usize,
    ptr: *mut u8,
    capacity: usize,
}

impl<'a> Spare<'a> {
    fn new(buf: &'a mut BytesMut) -> Self {
        let start = buf.len();
        let spare = buf.chunk_mut();
        let (ptr, capacity) = (spare.as_mut_ptr(), spare.len());
        Spare {
            buf,
            start,
            ptr,
            capacity,
        }
    }
}

// SAFETY: `ptr` points to `capacity` bytes of the buffer's spare capacity, and
// `filled_until` only extends the buffer over bytes zstd has written.
unsafe impl WriteBuf for Spare<'_> {
    fn as_slice(&self) -> &[u8] {
        &self.buf[self.start..]
    }

    fn capacity(&self) -> usize {
        self.capacity
    }

    fn as_mut_ptr(&mut self) -> *mut u8 {
        self.ptr
    }

    unsafe fn filled_until(&mut self, n: usize) {
        unsafe { self.buf.set_len(self.start + n) }
    }
}

#[cfg(test)]
mod tests {
    use flate2::{Compression, write::GzEncoder, write::ZlibEncoder};
    use futures_util::stream::{self, StreamExt};

    use super::*;

    const BOMB_SIZE: usize = 4 * 1024 * 1024;

    fn bomb(encoding: ContentEncoding) -> Vec<u8> {
        let data = vec![0u8; BOMB_SIZE];
        if encoding == ContentEncoding::Gzip {
            let mut e = GzEncoder::new(Vec::new(), Compression::best());
            e.write_all(&data).unwrap();
            e.finish().unwrap()
        } else {
            let mut e = ZlibEncoder::new(Vec::new(), Compression::best());
            e.write_all(&data).unwrap();
            e.finish().unwrap()
        }
    }

    #[crate::rt_test]
    async fn decoded_chunks_are_bounded() {
        for encoding in [ContentEncoding::Gzip, ContentEncoding::Deflate] {
            let compressed = bomb(encoding);
            assert!(compressed.len() < 32 * 1024);

            for size in [compressed.len(), 2048] {
                let chunks: Vec<_> = compressed
                    .chunks(size)
                    .map(|c| Ok::<_, PayloadError>(Bytes::copy_from_slice(c)))
                    .collect();
                let mut decoder = Decoder::new(stream::iter(chunks), encoding);

                let mut total = 0;
                let mut max = 0;
                while let Some(chunk) = decoder.next().await {
                    let chunk = chunk.unwrap();
                    assert!(chunk.iter().all(|b| *b == 0));
                    max = max.max(chunk.len());
                    total += chunk.len();
                }
                assert_eq!(total, BOMB_SIZE);
                assert!(
                    max <= 3 * MAX_CHUNK_SIZE,
                    "{encoding:?} chunk of {max} bytes"
                );
            }
        }
    }

    #[crate::rt_test]
    async fn decoder_is_fused_after_error() {
        let chunks = vec![
            Ok::<_, PayloadError>(Bytes::from_static(b"not gzip data")),
            Ok(Bytes::from_static(b"raw")),
        ];
        let mut decoder = Decoder::new(stream::iter(chunks), ContentEncoding::Gzip);
        assert!(matches!(decoder.next().await, Some(Err(_))));
        assert!(decoder.next().await.is_none());
    }

    #[crate::rt_test]
    async fn decoder_from_headers() {
        use crate::http::header::HeaderValue;

        let mut headers = HeaderMap::new();
        headers.insert(
            CONTENT_ENCODING,
            HeaderValue::from_bytes(b"gzip\xff").unwrap(),
        );
        let chunks = vec![Ok::<_, PayloadError>(Bytes::from_static(b"raw"))];
        let mut decoder = Decoder::from_headers(stream::iter(chunks), &headers);
        assert!(!decoder.is_decoding());
        assert_eq!(decoder.next().await.unwrap().unwrap(), "raw");
        assert!(decoder.next().await.is_none());
    }

    #[crate::rt_test]
    async fn decoder_error_on_blocking_pool() {
        let before = super::super::offloaded();
        let chunks = vec![Ok::<_, PayloadError>(Bytes::from(vec![b'x'; 256 * 1024]))];
        let mut decoder = Decoder::new(stream::iter(chunks), ContentEncoding::Deflate);
        assert!(matches!(decoder.next().await, Some(Err(_))));
        assert!(decoder.next().await.is_none());
        assert_eq!(super::super::offloaded(), before + 1);
    }

    fn compress(encoding: ContentEncoding, data: &[u8]) -> Vec<u8> {
        match encoding {
            ContentEncoding::Gzip => {
                let mut e = GzEncoder::new(Vec::new(), Compression::fast());
                e.write_all(data).unwrap();
                e.finish().unwrap()
            }
            ContentEncoding::Deflate => {
                let mut e = ZlibEncoder::new(Vec::new(), Compression::fast());
                e.write_all(data).unwrap();
                e.finish().unwrap()
            }
            ContentEncoding::Zstd => zstd::encode_all(data, 0).unwrap(),
            _ => unreachable!(),
        }
    }

    async fn decode(
        encoding: ContentEncoding,
        chunks: Vec<Bytes>,
    ) -> Result<Vec<u8>, PayloadError> {
        let chunks = chunks.into_iter().map(Ok::<_, PayloadError>);
        let mut decoder = Decoder::new(stream::iter(chunks), encoding);
        let mut out = Vec::new();
        while let Some(chunk) = decoder.next().await {
            let chunk = chunk?;
            assert!(!chunk.is_empty());
            assert!(chunk.len() <= 3 * MAX_CHUNK_SIZE);
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    }

    /// Incompressible data, so the compressed size is close to `len`.
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
    async fn decoder_offloads_large_input() {
        for (encoding, limit) in [
            (ContentEncoding::Gzip, 128 * 1024),
            (ContentEncoding::Deflate, 128 * 1024),
            (ContentEncoding::Zstd, 512 * 1024),
        ] {
            let data = random(limit + 1024);
            let compressed = compress(encoding, &data);
            assert!(compressed.len() > limit);

            // input below the limit is decoded in place
            let before = super::super::offloaded();
            let chunks = compressed
                .chunks(limit - 1)
                .map(Bytes::copy_from_slice)
                .collect();
            assert_eq!(decode(encoding, chunks).await.unwrap(), data);
            assert_eq!(super::super::offloaded(), before, "{encoding:?}");

            // the first feed of a larger chunk is offloaded
            let chunks = compressed[..limit]
                .chunks(limit)
                .chain(compressed[limit..].chunks(limit))
                .map(Bytes::copy_from_slice)
                .collect();
            assert_eq!(decode(encoding, chunks).await.unwrap(), data);
            assert_eq!(super::super::offloaded(), before + 1, "{encoding:?}");
        }
    }

    #[crate::rt_test]
    async fn zstd_decoder() {
        let data = random(300 * 1024);
        let compressed = zstd::encode_all(&data[..], 0).unwrap();
        for size in [1, 100, 64 * 1024, compressed.len()] {
            let chunks = compressed
                .chunks(size)
                .map(Bytes::copy_from_slice)
                .collect();
            assert_eq!(decode(ContentEncoding::Zstd, chunks).await.unwrap(), data);
        }

        // concatenated frames
        let mut frames = zstd::encode_all(&b"hello "[..], 0).unwrap();
        frames.extend(zstd::encode_all(&b"world"[..], 0).unwrap());
        let out = decode(ContentEncoding::Zstd, vec![Bytes::from(frames)]).await;
        assert_eq!(out.unwrap(), b"hello world");
    }

    #[crate::rt_test]
    async fn zstd_decoder_drains_output_without_input() {
        let data = b"hello world ".repeat(50_000);
        let compressed = zstd::encode_all(&data[..], 0).unwrap();

        // find the input byte that completes the first block
        let mut raw = raw::Decoder::new().unwrap();
        let mut out = vec![0; data.len()];
        let mut pos = 0;
        let mut split = 0;
        for (i, b) in compressed.iter().enumerate() {
            let mut src = InBuffer::around(std::slice::from_ref(b));
            let mut dst = OutBuffer::around_pos(&mut out[..], pos);
            raw.run(&mut src, &mut dst).unwrap();
            if dst.pos() - pos > MAX_CHUNK_SIZE {
                pos = dst.pos();
                split = i + 1;
                break;
            }
            pos = dst.pos();
        }
        assert!(split > 0 && split < compressed.len());

        // the stream stalls after the block, its output must not wait for more input
        let chunks = compressed[..split]
            .chunks(1)
            .map(|c| Ok::<_, PayloadError>(Bytes::copy_from_slice(c)))
            .collect::<Vec<_>>();
        let stream = stream::iter(chunks).chain(stream::pending());
        let mut decoder = Decoder::new(stream, ContentEncoding::Zstd);

        let mut decoded = Vec::new();
        while decoded.len() < pos {
            let chunk = crate::time::timeout(crate::time::Millis(5_000), decoder.next())
                .await
                .expect("decoder is stuck")
                .unwrap()
                .unwrap();
            assert!(chunk.len() <= MAX_CHUNK_SIZE);
            decoded.extend_from_slice(&chunk);
        }
        assert_eq!(decoded, data[..pos]);
    }

    #[crate::rt_test]
    async fn zstd_decoded_chunks_are_bounded() {
        let compressed = zstd::encode_all(&vec![0u8; BOMB_SIZE][..], 19).unwrap();
        assert!(compressed.len() < 1024);

        for size in [1, compressed.len()] {
            let chunks: Vec<_> = compressed
                .chunks(size)
                .map(|c| Ok::<_, PayloadError>(Bytes::copy_from_slice(c)))
                .collect();
            let mut decoder = Decoder::new(stream::iter(chunks), ContentEncoding::Zstd);
            let mut total = 0;
            while let Some(chunk) = decoder.next().await {
                let chunk = chunk.unwrap();
                assert!(chunk.iter().all(|b| *b == 0));
                assert!(chunk.len() <= BytePageSize::Size32.capacity());
                total += chunk.len();
            }
            assert_eq!(total, BOMB_SIZE);
        }
    }

    #[crate::rt_test]
    async fn zstd_decoder_errors() {
        let compressed = zstd::encode_all(&random(1024)[..], 0).unwrap();

        // truncated
        let chunks = vec![Bytes::copy_from_slice(&compressed[..compressed.len() - 4])];
        assert!(decode(ContentEncoding::Zstd, chunks).await.is_err());

        // empty body
        assert!(decode(ContentEncoding::Zstd, vec![]).await.is_err());

        // corrupted
        let out = decode(ContentEncoding::Zstd, vec![Bytes::from_static(b"not zstd")]).await;
        assert!(out.is_err());

        // a window above 8MiB
        let mut e = zstd::stream::write::Encoder::new(Vec::new(), 0).unwrap();
        e.set_parameter(zstd::stream::raw::CParameter::WindowLog(24))
            .unwrap();
        e.write_all(b"hello").unwrap();
        let large = e.finish().unwrap();
        let out = decode(ContentEncoding::Zstd, vec![Bytes::from(large)]).await;
        assert!(out.is_err());

        // the same window size is accepted at 8MiB
        let mut e = zstd::stream::write::Encoder::new(Vec::new(), 0).unwrap();
        e.set_parameter(zstd::stream::raw::CParameter::WindowLog(23))
            .unwrap();
        e.write_all(b"hello").unwrap();
        let ok = e.finish().unwrap();
        let out = decode(ContentEncoding::Zstd, vec![Bytes::from(ok)]).await;
        assert_eq!(out.unwrap(), b"hello");
    }

    #[crate::rt_test]
    async fn decoder_truncated_stream() {
        let mut e = GzEncoder::new(Vec::new(), Compression::fast());
        e.write_all(b"hello world").unwrap();
        let data = e.finish().unwrap();

        let chunks = vec![Ok::<_, PayloadError>(Bytes::copy_from_slice(
            &data[..data.len() - 4],
        ))];
        let mut decoder = Decoder::new(stream::iter(chunks), ContentEncoding::Gzip);
        let mut result = Ok(());
        while let Some(chunk) = decoder.next().await {
            if let Err(e) = chunk {
                result = Err(e);
            }
        }
        assert!(result.is_err());
    }
}
