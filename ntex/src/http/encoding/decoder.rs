use std::sync::atomic::{AtomicUsize, Ordering};
use std::{collections::VecDeque, future::Future, io, pin::Pin, task::Context, task::Poll};

use flate2::{Crc, Decompress, FlushDecompress, Status};
use zstd::zstd_safe::{DCtx, DParameter, InBuffer, OutBuffer};

use super::{Spare, offload};
use crate::http::error::PayloadError;
use crate::http::header::{CONTENT_ENCODING, ContentEncoding, HeaderMap};
use crate::rt::BlockingResult;
use crate::util::{BufMut, BytePageSize, Bytes, BytesMut, Stream};

/// Decoders write into pages of this size, so decoded chunks are never larger.
#[cfg(test)]
const MAX_CHUNK_SIZE: usize = 32 * 1024;

/// A blocking task stops decoding once its output reaches this size.
///
/// The output is kept in chunks of at most 32KiB, a single page.
const MAX_TASK_OUTPUT: usize = 256 * 1024;

/// The largest `zstd` window a decoder accepts, 8MiB as required by RFC 9659.
const ZSTD_WINDOW_LOG_MAX: u32 = 23;

/// Memory all `zstd` decoders of the process may use beyond `ZSTD_FREE_MEMORY` each.
///
/// RFC 9659 requires 8MiB windows, so a request can make its decoder allocate
/// about 9MiB. A frame that would take the total over this limit is rejected.
const ZSTD_MEMORY_LIMIT: usize = 512 * 1024 * 1024;

/// Memory a `zstd` decoder uses without being counted, enough for a 512KiB window.
const ZSTD_FREE_MEMORY: usize = 1024 * 1024;

static ZSTD_MEMORY: ZstdMemory = ZstdMemory::new(ZSTD_MEMORY_LIMIT);

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
    /// Output of a blocking task that is not returned yet
    #[debug(skip)]
    ready: VecDeque<Bytes>,
    #[debug(skip)]
    fut: Option<BlockingResult<DecodeResult>>,
}

type DecodeResult = Result<(VecDeque<Bytes>, ContentDecoder, Bytes), io::Error>;

impl<S> Decoder<S>
where
    S: Stream<Item = Result<Bytes, PayloadError>>,
{
    /// Construct a decoder for the given content encoding.
    #[inline]
    pub fn new(stream: S, encoding: ContentEncoding) -> Decoder<S> {
        let inner = match encoding {
            ContentEncoding::Deflate => {
                Some(ContentDecoder::Flate(Box::new(FlateDecoder::new(false))))
            }
            ContentEncoding::Gzip => Some(ContentDecoder::Flate(Box::new(FlateDecoder::new(true)))),
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
            ready: VecDeque::new(),
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
            if let Some(chunk) = self.ready.pop_front() {
                return Poll::Ready(Some(Ok(chunk)));
            }

            if let Some(ref mut fut) = self.fut {
                let (chunks, decoder, rest) = match Pin::new(fut).poll(cx) {
                    Poll::Ready(Ok(Ok(item))) => item,
                    Poll::Ready(Ok(Err(e))) => return Poll::Ready(Some(Err(e.into()))),
                    Poll::Ready(Err(e)) => return Poll::Ready(Some(Err(e.into()))),
                    Poll::Pending => return Poll::Pending,
                };
                self.fut = None;
                self.restore(decoder, rest);
                self.ready = chunks;
                continue;
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
                        let chunks = decoder.feed_task(&mut data)?;
                        Ok((chunks, decoder, data))
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
    Flate(Box<FlateDecoder>),
    Zstd(Box<ZstdDecoder>),
}

impl ContentDecoder {
    /// Input of this size and larger is decoded on the blocking thread pool.
    ///
    /// A feed on the current thread decodes a single page of output, a task
    /// on the pool decodes several pages, up to `MAX_TASK_OUTPUT`.
    const fn limit(&self) -> usize {
        match self {
            ContentDecoder::Flate(_) => 128 * 1024,
            ContentDecoder::Zstd(_) => 512 * 1024,
        }
    }

    /// Returns `true` if decoded output is left without new input.
    fn has_more(&self) -> bool {
        match self {
            ContentDecoder::Flate(decoder) => decoder.more,
            ContentDecoder::Zstd(decoder) => decoder.more,
        }
    }

    fn feed_eof(&mut self) -> io::Result<Option<Bytes>> {
        match self {
            ContentDecoder::Flate(decoder) => decoder.finish().map(|()| None),
            ContentDecoder::Zstd(decoder) => decoder.finish().map(|()| None),
        }
    }

    /// Decodes `data` on a blocking task until the output reaches `MAX_TASK_OUTPUT`.
    fn feed_task(&mut self, data: &mut Bytes) -> io::Result<VecDeque<Bytes>> {
        let mut chunks = VecDeque::new();
        let mut size = 0;
        while size < MAX_TASK_OUTPUT && !data.is_empty() {
            let Some(chunk) = self.feed_data(data)? else {
                break;
            };
            size += chunk.len();
            chunks.push_back(chunk);
        }
        Ok(chunks)
    }

    /// Decodes `data` until a page of output is full.
    ///
    /// Decoded input is removed from `data`.
    fn feed_data(&mut self, data: &mut Bytes) -> io::Result<Option<Bytes>> {
        match self {
            ContentDecoder::Flate(decoder) => decoder.feed(data),
            ContentDecoder::Zstd(decoder) => decoder.feed(data),
        }
    }
}

/// `deflate` (zlib) or `gzip` decoder.
struct FlateDecoder {
    inner: Decompress,
    buf: BytesMut,
    /// The `gzip` header and trailer, `None` for `deflate`
    gzip: Option<Gzip>,
    /// The compressed data is complete
    done: bool,
    /// The output buffer was filled, the decoder may hold more output
    more: bool,
}

impl FlateDecoder {
    fn new(gzip: bool) -> Self {
        FlateDecoder {
            inner: Decompress::new(!gzip),
            buf: BytesMut::with_page_size(BytePageSize::Size32),
            gzip: gzip.then(Gzip::new),
            done: false,
            more: false,
        }
    }

    /// Decodes `data` until the output buffer is full.
    fn feed(&mut self, data: &mut Bytes) -> io::Result<Option<Bytes>> {
        if let Some(gzip) = &mut self.gzip
            && !gzip.header(data)?
        {
            return Ok(None);
        }

        self.buf.reserve_more();
        let start = self.buf.len();
        while !self.done && self.buf.remaining_mut() > 0 && (!data.is_empty() || self.more) {
            let (total_in, total_out) = (self.inner.total_in(), self.inner.total_out());
            // SAFETY: the decoder only writes to the slice, and `advance_mut`
            // covers just the bytes it has written
            let status = unsafe {
                let spare = self.buf.chunk_mut().as_uninit_slice_mut();
                self.inner
                    .decompress_uninit(data, spare, FlushDecompress::None)
                    .map_err(invalid_data)?
            };
            let read = usize::try_from(self.inner.total_in() - total_in).unwrap();
            let written = usize::try_from(self.inner.total_out() - total_out).unwrap();
            unsafe { self.buf.advance_mut(written) };
            data.advance_to(read);

            self.done = status == Status::StreamEnd;
            self.more = !self.done && self.buf.remaining_mut() == 0;
            if read == 0 && written == 0 {
                break;
            }
        }

        if let Some(gzip) = &mut self.gzip {
            gzip.crc.update(&self.buf[start..]);
            if self.done {
                gzip.trailer(data)?;
            }
        }
        if self.done && !data.is_empty() {
            return Err(invalid_data("data after the end of the stream"));
        }

        if self.buf.is_empty() {
            Ok(None)
        } else {
            Ok(Some(self.buf.take()))
        }
    }

    fn finish(&self) -> io::Result<()> {
        if self.done
            && self
                .gzip
                .as_ref()
                .is_none_or(|gzip| gzip.state == GzState::Done)
        {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "compressed stream is incomplete",
            ))
        }
    }
}

const FHCRC: u8 = 0x02;
const FEXTRA: u8 = 0x04;
const FNAME: u8 = 0x08;
const FCOMMENT: u8 = 0x10;
const FRESERVED: u8 = 0xe0;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum GzState {
    Header,
    ExtraLen,
    Extra(usize),
    Name,
    Comment,
    HeaderCrc,
    Body,
    Trailer,
    Done,
}

/// Parser of the `gzip` member header and trailer, RFC 1952.
///
/// The optional header fields are skipped. Like most decoders, only a single
/// member is accepted.
struct Gzip {
    state: GzState,
    flags: u8,
    /// Checksum of the header
    header_crc: Crc,
    /// Checksum and size of the decoded data
    crc: Crc,
    buf: [u8; 10],
    len: usize,
}

impl Gzip {
    fn new() -> Self {
        Gzip {
            state: GzState::Header,
            flags: 0,
            header_crc: Crc::new(),
            crc: Crc::new(),
            buf: [0; 10],
            len: 0,
        }
    }

    /// Moves up to `n` bytes of `data` to `buf`, returns `true` once it holds `n` bytes.
    fn fill(&mut self, data: &mut Bytes, n: usize) -> bool {
        let size = (n - self.len).min(data.len());
        self.buf[self.len..self.len + size].copy_from_slice(&data[..size]);
        self.len += size;
        data.advance_to(size);
        self.len == n
    }

    /// Moves to `state`, the bytes of the previous field are dropped.
    fn next(&mut self, state: GzState) {
        self.state = state;
        self.len = 0;
    }

    /// Parses the header, returns `true` once it is complete.
    fn header(&mut self, data: &mut Bytes) -> io::Result<bool> {
        loop {
            match self.state {
                GzState::Header => {
                    if !self.fill(data, 10) {
                        return Ok(false);
                    }
                    let [id1, id2, method, flags, ..] = self.buf;
                    if id1 != 0x1f || id2 != 0x8b {
                        return Err(invalid_data("invalid gzip header"));
                    }
                    if method != 8 || flags & FRESERVED != 0 {
                        return Err(invalid_data("unsupported gzip header"));
                    }
                    self.flags = flags;
                    self.header_crc.update(&self.buf);
                    self.next(GzState::ExtraLen);
                }
                GzState::ExtraLen if self.flags & FEXTRA == 0 => self.next(GzState::Name),
                GzState::ExtraLen => {
                    if !self.fill(data, 2) {
                        return Ok(false);
                    }
                    let len = [self.buf[0], self.buf[1]];
                    self.header_crc.update(&len);
                    self.next(GzState::Extra(u16::from_le_bytes(len).into()));
                }
                GzState::Extra(len) => {
                    let size = len.min(data.len());
                    self.header_crc.update(&data[..size]);
                    data.advance_to(size);
                    if size < len {
                        self.state = GzState::Extra(len - size);
                        return Ok(false);
                    }
                    self.next(GzState::Name);
                }
                GzState::Name if self.flags & FNAME == 0 => self.next(GzState::Comment),
                GzState::Comment if self.flags & FCOMMENT == 0 => self.next(GzState::HeaderCrc),
                GzState::Name | GzState::Comment => {
                    // a zero-terminated string
                    let Some(end) = data.iter().position(|b| *b == 0) else {
                        self.header_crc.update(data);
                        data.advance_to(data.len());
                        return Ok(false);
                    };
                    self.header_crc.update(&data[..=end]);
                    data.advance_to(end + 1);
                    self.next(if self.state == GzState::Name {
                        GzState::Comment
                    } else {
                        GzState::HeaderCrc
                    });
                }
                GzState::HeaderCrc if self.flags & FHCRC == 0 => self.next(GzState::Body),
                GzState::HeaderCrc => {
                    if !self.fill(data, 2) {
                        return Ok(false);
                    }
                    // the low 16 bits of the crc32
                    let crc = u16::from_le_bytes([self.buf[0], self.buf[1]]);
                    if u32::from(crc) != self.header_crc.sum() & 0xffff {
                        return Err(invalid_data("gzip header checksum mismatch"));
                    }
                    self.next(GzState::Body);
                }
                GzState::Body | GzState::Trailer | GzState::Done => return Ok(true),
            }
        }
    }

    /// Checks the trailer after the compressed data, the crc32 and size of the output.
    fn trailer(&mut self, data: &mut Bytes) -> io::Result<()> {
        if self.state == GzState::Body {
            self.next(GzState::Trailer);
        }
        if self.state == GzState::Trailer && self.fill(data, 8) {
            let [c0, c1, c2, c3, s0, s1, s2, s3, ..] = self.buf;
            if u32::from_le_bytes([c0, c1, c2, c3]) != self.crc.sum() {
                return Err(invalid_data("gzip checksum mismatch"));
            }
            // the size modulo 2^32
            if u32::from_le_bytes([s0, s1, s2, s3]) != self.crc.amount() {
                return Err(invalid_data("gzip size mismatch"));
            }
            self.next(GzState::Done);
        }
        Ok(())
    }
}

fn invalid_data<E>(err: E) -> io::Error
where
    E: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    io::Error::new(io::ErrorKind::InvalidData, err)
}

/// Memory used by `zstd` decoders.
struct ZstdMemory {
    used: AtomicUsize,
    limit: usize,
}

impl ZstdMemory {
    const fn new(limit: usize) -> Self {
        ZstdMemory {
            used: AtomicUsize::new(0),
            limit,
        }
    }

    fn acquire(&self, size: usize) -> io::Result<()> {
        self.used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(size).filter(|total| *total <= self.limit)
            })
            .map(|_| ())
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::OutOfMemory,
                    "zstd decoders memory limit is reached",
                )
            })
    }

    fn release(&self, size: usize) {
        self.used.fetch_sub(size, Ordering::AcqRel);
    }
}

struct ZstdDecoder {
    ctx: DCtx<'static>,
    buf: BytesMut,
    memory: &'static ZstdMemory,
    /// Memory counted in `memory`
    charged: usize,
    /// The last frame is complete
    done: bool,
    /// The output buffer was filled, the decoder may hold more output
    more: bool,
}

impl ZstdDecoder {
    fn new() -> io::Result<Self> {
        Self::with_memory(&ZSTD_MEMORY)
    }

    fn with_memory(memory: &'static ZstdMemory) -> io::Result<Self> {
        let mut ctx = DCtx::try_create().ok_or(io::ErrorKind::OutOfMemory)?;
        ctx.set_parameter(DParameter::WindowLogMax(ZSTD_WINDOW_LOG_MAX))
            .map_err(zstd_error)?;
        Ok(ZstdDecoder {
            ctx,
            memory,
            charged: 0,
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
            let hint = self
                .ctx
                .decompress_stream(&mut dst, &mut src)
                .map_err(zstd_error)?;
            let (read, written) = (src.pos(), dst.pos());
            self.charge()?;
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

    /// Counts the memory of the context, it grows with the window of a frame.
    fn charge(&mut self) -> io::Result<()> {
        let size = self.ctx.sizeof().saturating_sub(ZSTD_FREE_MEMORY);
        if size > self.charged {
            self.memory.acquire(size - self.charged)?;
        } else {
            self.memory.release(self.charged - size);
        }
        self.charged = size;
        Ok(())
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

impl Drop for ZstdDecoder {
    fn drop(&mut self) {
        self.memory.release(self.charged);
    }
}

fn zstd_error(code: usize) -> io::Error {
    io::Error::other(zstd::zstd_safe::get_error_name(code))
}

#[cfg(test)]
mod tests {
    use std::io::Write;

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
                assert!(max <= MAX_CHUNK_SIZE, "{encoding:?} chunk of {max} bytes");
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
            assert!(chunk.len() <= MAX_CHUNK_SIZE);
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
        use zstd::stream::raw::Operation;

        let data = b"hello world ".repeat(50_000);
        let compressed = zstd::encode_all(&data[..], 0).unwrap();

        // find the input byte that completes the first block
        let mut raw = zstd::stream::raw::Decoder::new().unwrap();
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

    /// A frame with an unknown content size, its decoder allocates the whole window.
    fn zstd_frame(window_log: u32, data: &[u8]) -> Vec<u8> {
        let mut e = zstd::stream::write::Encoder::new(Vec::new(), 0).unwrap();
        e.set_parameter(zstd::stream::raw::CParameter::WindowLog(window_log))
            .unwrap();
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    fn zstd_feed(decoder: &mut ZstdDecoder, frame: &[u8]) -> io::Result<Vec<u8>> {
        let mut data = Bytes::copy_from_slice(frame);
        let mut out = Vec::new();
        while !data.is_empty() || decoder.more {
            if let Some(chunk) = decoder.feed(&mut data)? {
                out.extend_from_slice(&chunk);
            }
        }
        Ok(out)
    }

    #[test]
    fn zstd_decoder_memory_is_limited() {
        static MEMORY: ZstdMemory = ZstdMemory::new(20 * 1024 * 1024);

        let large = zstd_frame(23, b"hello");
        let mut first = ZstdDecoder::with_memory(&MEMORY).unwrap();
        assert_eq!(zstd_feed(&mut first, &large).unwrap(), b"hello");
        let charged = first.charged;
        assert!(charged > 7 * 1024 * 1024, "{charged}");
        assert_eq!(MEMORY.used.load(Ordering::Acquire), charged);

        let mut second = ZstdDecoder::with_memory(&MEMORY).unwrap();
        assert_eq!(zstd_feed(&mut second, &large).unwrap(), b"hello");
        assert_eq!(MEMORY.used.load(Ordering::Acquire), 2 * charged);

        // the third decoder is over the limit
        let mut third = ZstdDecoder::with_memory(&MEMORY).unwrap();
        let err = zstd_feed(&mut third, &large).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::OutOfMemory);
        drop(third);
        assert_eq!(MEMORY.used.load(Ordering::Acquire), 2 * charged);

        // small windows are not counted
        let mut small = ZstdDecoder::with_memory(&MEMORY).unwrap();
        assert_eq!(
            zstd_feed(&mut small, &zstd_frame(19, b"hi")).unwrap(),
            b"hi"
        );
        assert_eq!(small.charged, 0);

        // memory of a dropped decoder is available again
        drop(first);
        assert_eq!(MEMORY.used.load(Ordering::Acquire), charged);
        let mut third = ZstdDecoder::with_memory(&MEMORY).unwrap();
        assert_eq!(zstd_feed(&mut third, &large).unwrap(), b"hello");
        drop((second, third));
        assert_eq!(MEMORY.used.load(Ordering::Acquire), 0);
    }

    #[test]
    fn zstd_decoder_memory_shrinks() {
        static MEMORY: ZstdMemory = ZstdMemory::new(20 * 1024 * 1024);

        // zstd frees an oversized window after it was unused for a number of frames
        let mut decoder = ZstdDecoder::with_memory(&MEMORY).unwrap();
        zstd_feed(&mut decoder, &zstd_frame(23, b"hello")).unwrap();
        assert!(decoder.charged > 0);
        let small = zstd_frame(10, b"hi");
        for _ in 0..256 {
            zstd_feed(&mut decoder, &small).unwrap();
        }
        assert_eq!(decoder.charged, 0);
        assert_eq!(MEMORY.used.load(Ordering::Acquire), 0);
    }

    #[crate::rt_test]
    async fn zstd_decoder_memory_error() {
        static MEMORY: ZstdMemory = ZstdMemory::new(1024 * 1024);

        let chunks = vec![Ok::<_, PayloadError>(Bytes::from(zstd_frame(23, b"hello")))];
        let mut decoder = Decoder {
            inner: Some(ContentDecoder::Zstd(Box::new(
                ZstdDecoder::with_memory(&MEMORY).unwrap(),
            ))),
            stream: stream::iter(chunks),
            eof: false,
            decode: true,
            pending: None,
            ready: VecDeque::new(),
            fut: None,
        };
        assert!(matches!(decoder.next().await, Some(Err(_))));
        assert!(decoder.next().await.is_none());
        assert_eq!(MEMORY.used.load(Ordering::Acquire), 0);
    }

    #[crate::rt_test]
    async fn decoder_task_output() {
        for (encoding, limit) in [
            (ContentEncoding::Gzip, 128 * 1024),
            (ContentEncoding::Zstd, 512 * 1024),
        ] {
            let data = random(2 * 1024 * 1024);
            let compressed = compress(encoding, &data);
            let before = super::super::offloaded();
            let chunks = vec![Bytes::from(compressed)];
            assert_eq!(decode(encoding, chunks).await.unwrap(), data);

            // each task decodes MAX_TASK_OUTPUT, the rest below the limit in place
            let tasks = super::super::offloaded() - before;
            let expected = (data.len() - limit).div_ceil(MAX_TASK_OUTPUT);
            assert!(
                (expected - 1..=expected).contains(&tasks),
                "{encoding:?} {tasks}"
            );
        }
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

    fn bytewise(data: &[u8]) -> Vec<Bytes> {
        data.chunks(1).map(Bytes::copy_from_slice).collect()
    }

    /// A gzip member with all optional header fields, the header crc is at `len - 2`.
    fn gzip_header(flags: u8) -> Vec<u8> {
        let mut header = vec![0x1f, 0x8b, 8, flags, 1, 2, 3, 4, 0, 255];
        header.extend([3, 0, b'a', b'b', b'c']);
        header.extend(b"name\0comment\0");
        let mut crc = flate2::Crc::new();
        crc.update(&header);
        header.extend((crc.sum() as u16).to_le_bytes());
        header
    }

    #[crate::rt_test]
    async fn gzip_decoder_header() {
        let data = random(4096);
        let body = &compress(ContentEncoding::Gzip, &data)[10..];
        let mut full = gzip_header(0x1e);
        full.extend(body);
        for chunks in [vec![Bytes::from(full.clone())], bytewise(&full)] {
            assert_eq!(decode(ContentEncoding::Gzip, chunks).await.unwrap(), data);
        }

        // header crc, magic, method and reserved flags
        let crc = gzip_header(0x1e).len() - 2;
        for (pos, flip) in [(crc, 1), (0, 1), (1, 1), (2, 1), (3, 0x20), (3, 0x80)] {
            let mut bad = full.clone();
            bad[pos] ^= flip;
            let out = decode(ContentEncoding::Gzip, vec![Bytes::from(bad)]).await;
            assert!(out.is_err(), "{pos} {flip}");
        }
    }

    #[crate::rt_test]
    async fn flate_decoder_errors() {
        let data = random(4096);
        for encoding in [ContentEncoding::Gzip, ContentEncoding::Deflate] {
            let compressed = compress(encoding, &data);
            let len = compressed.len();
            assert_eq!(decode(encoding, bytewise(&compressed)).await.unwrap(), data);

            let mut trailing = compressed.clone();
            trailing.push(0);
            let out = decode(encoding, vec![Bytes::from(trailing)]).await;
            assert!(out.is_err(), "{encoding:?} trailing data");

            // checksums
            for pos in [len - 1, len - 5] {
                let mut bad = compressed.clone();
                bad[pos] ^= 1;
                let out = decode(encoding, vec![Bytes::from(bad)]).await;
                assert!(out.is_err(), "{encoding:?} {pos}");
            }

            for n in [0, 1, 11, len - 1] {
                let chunks = vec![Bytes::copy_from_slice(&compressed[..n])];
                let out = decode(encoding, chunks).await;
                assert!(out.is_err(), "{encoding:?} truncated to {n}");
            }
        }
    }

    #[test]
    fn flate_decoder_output_fills_pages() {
        // some sizes end the stream exactly at the end of a page, which holds
        // a little less than `MAX_CHUNK_SIZE`
        let mut full = 0;
        for gzip in [false, true] {
            let encoding = if gzip {
                ContentEncoding::Gzip
            } else {
                ContentEncoding::Deflate
            };
            for len in MAX_CHUNK_SIZE - 64..=MAX_CHUNK_SIZE {
                let data = b"abc".repeat(len)[..len].to_vec();
                let mut input = Bytes::from(compress(encoding, &data));
                let mut decoder = FlateDecoder::new(gzip);
                let mut out = Vec::new();
                while !input.is_empty() || decoder.more {
                    if let Some(chunk) = decoder.feed(&mut input).unwrap() {
                        out.extend_from_slice(&chunk);
                    }
                    full += usize::from(decoder.done && decoder.buf.remaining_mut() == 0);
                }
                decoder.finish().unwrap();
                assert_eq!(out, data);
            }
        }
        assert!(full > 0);
    }
}
