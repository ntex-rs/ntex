use std::{future::Future, io, io::Write, pin::Pin, task::Context, task::Poll};

use flate2::write::{GzDecoder, ZlibDecoder};

use super::Writer;
use crate::http::error::PayloadError;
use crate::http::header::{CONTENT_ENCODING, ContentEncoding, HeaderMap};
use crate::rt::{BlockingResult, spawn_blocking};
use crate::util::{Bytes, Stream};

const INPLACE: usize = 2049;

/// Decoding stops at this output size, the rest of the input is decoded by the next poll.
///
/// A single write or flush of the decoder adds at most 32KiB, so chunks stay below 96KiB.
const MAX_CHUNK_SIZE: usize = 32 * 1024;

/// Payload stream decoder.
///
/// Decompresses a stream of payload chunks. `gzip` and `deflate` are decoded;
/// other encodings pass the stream through unchanged.
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
    fn poll_decoded(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<Bytes, PayloadError>>> {
        loop {
            if let Some(ref mut fut) = self.fut {
                let (chunk, decoder, rest) = match Pin::new(fut).poll(cx) {
                    Poll::Ready(Ok(Ok(item))) => item,
                    Poll::Ready(Ok(Err(e))) => return Poll::Ready(Some(Err(e.into()))),
                    Poll::Ready(Err(e)) => return Poll::Ready(Some(Err(e.into()))),
                    Poll::Pending => return Poll::Pending,
                };
                self.inner = Some(decoder);
                self.fut = None;
                if !rest.is_empty() {
                    self.pending = Some(rest);
                }
                if let Some(chunk) = chunk {
                    return Poll::Ready(Some(Ok(chunk)));
                }
            }

            if let Some(mut data) = self.pending.take() {
                let mut decoder = self.inner.take().unwrap();
                if data.len() < INPLACE {
                    let chunk = decoder.feed_data(&mut data)?;
                    self.inner = Some(decoder);
                    if !data.is_empty() {
                        self.pending = Some(data);
                    }
                    if let Some(chunk) = chunk {
                        return Poll::Ready(Some(Ok(chunk)));
                    }
                } else {
                    self.fut = Some(spawn_blocking(move || {
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
}

impl ContentDecoder {
    fn feed_eof(&mut self) -> io::Result<Option<Bytes>> {
        match self {
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
        while !data.is_empty() && self.output_len() < MAX_CHUNK_SIZE {
            let n = match self {
                ContentDecoder::Gzip(decoder) => decoder.write(data)?,
                ContentDecoder::Deflate(decoder) => decoder.write(data)?,
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
            }
        }

        let b = match self {
            ContentDecoder::Gzip(decoder) => decoder.get_mut().take(),
            ContentDecoder::Deflate(decoder) => decoder.get_mut().take(),
        };
        if b.is_empty() { Ok(None) } else { Ok(Some(b)) }
    }

    fn output_len(&self) -> usize {
        match self {
            ContentDecoder::Gzip(decoder) => decoder.get_ref().len(),
            ContentDecoder::Deflate(decoder) => decoder.get_ref().len(),
        }
    }
}

#[cfg(test)]
mod tests {
    use flate2::{Compression, write::GzEncoder, write::ZlibEncoder};
    use futures_util::stream::{self, StreamExt};

    use super::*;

    const BOMB_SIZE: usize = 16 * 1024 * 1024;

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

            // a single chunk is decoded on the blocking pool, small chunks in place
            for size in [compressed.len(), INPLACE - 1] {
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
        let chunks = vec![Ok::<_, PayloadError>(Bytes::from(vec![b'x'; INPLACE * 2]))];
        let mut decoder = Decoder::new(stream::iter(chunks), ContentEncoding::Deflate);
        assert!(matches!(decoder.next().await, Some(Err(_))));
        assert!(decoder.next().await.is_none());
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
