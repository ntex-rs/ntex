use std::pin::Pin;
use std::task::{Context, Poll};
use std::{fmt, io};
use std::io::IoSlice;

use ntex_bytes::{BufferAccumulator, BufferPool, BufferSource, BytesPool};
use ntex_util::{future::Either, ready};

use crate::{AsyncRead, AsyncWrite, Decoder, Encoder};

const HW: usize = 8 * 1024;
const NUM_VECTORED_IO_SLICES: usize = 8;

bitflags::bitflags! {
    struct Flags: u8 {
        const EOF          = 0b0001;
        const READABLE     = 0b0010;
        const DISCONNECTED = 0b0100;
        const SHUTDOWN     = 0b1000;
    }
}

/// A unified interface to an underlying I/O object, using
/// the `Encoder` and `Decoder` traits to encode and decode frames.
/// `Framed` is heavily optimized for streaming io.
pub struct Framed<T, U, P = BytesPool> where P: BufferPool {
    io: T,
    codec: U,
    flags: Flags,
    pool: P,
    read_buf: <P as BufferSource>::Owned,
    write_buf: P::Accumulator,
    err: Option<io::Error>,
}

impl<T, U, P> Framed<T, U, P>
where
    T: AsyncRead + AsyncWrite,
    U: Decoder<P> + Encoder<P>,
    P: BufferPool,
{
    /// Provides an interface for reading and writing to
    /// `Io` object, using `Decode` and `Encode` traits of codec.
    ///
    /// Raw I/O objects work with byte sequences, but higher-level code usually
    /// wants to batch these into meaningful chunks, called "frames". This
    /// method layers framing on top of an I/O object, by using the `Codec`
    /// traits to handle encoding and decoding of messages frames. Note that
    /// the incoming and outgoing frame types may be distinct.
    pub async fn new(io: T, codec: U, pool: P) -> Framed<T, U, P> {
        let read_buf = pool.take(HW).await;
        let write_buf = P::Accumulator::with_capacity(HW);
        Framed {
            io,
            codec,
            flags: Flags::empty(),
            pool,
            read_buf,
            write_buf,
            err: None,
        }
    }
}

impl<T, U, P> Framed<T, U, P> where P: BufferPool {
    #[inline]
    /// Construct `Framed` object `parts`.
    pub fn from_parts(parts: FramedParts<T, U, P>) -> Framed<T, U, P> {
        Framed {
            io: parts.io,
            codec: parts.codec,
            flags: parts.flags,
            pool: parts.pool,
            read_buf: parts.read_buf,
            write_buf: parts.write_buf,
            err: parts.err,
        }
    }

    #[inline]
    /// Returns a reference to the underlying codec.
    pub fn get_codec(&self) -> &U {
        &self.codec
    }

    #[inline]
    /// Returns a mutable reference to the underlying codec.
    pub fn get_codec_mut(&mut self) -> &mut U {
        &mut self.codec
    }

    #[inline]
    /// Returns a reference to the underlying I/O stream wrapped by `Framed`.
    ///
    /// Note that care should be taken to not tamper with the underlying stream
    /// of data coming in as it may corrupt the stream of frames otherwise
    /// being worked with.
    pub fn get_ref(&self) -> &T {
        &self.io
    }

    #[inline]
    /// Returns a mutable reference to the underlying I/O stream wrapped by
    /// `Framed`.
    ///
    /// Note that care should be taken to not tamper with the underlying stream
    /// of data coming in as it may corrupt the stream of frames otherwise
    /// being worked with.
    pub fn get_mut(&mut self) -> &mut T {
        &mut self.io
    }

    #[inline]
    /// Get read buffer.
    pub fn read_buf(&mut self) -> &mut <P as BufferSource>::Owned {
        &mut self.read_buf
    }

    #[inline]
    /// Get write buffer.
    pub fn write_buf(&mut self) -> &mut P::Accumulator {
        &mut self.write_buf
    }

    #[inline]
    /// Check if write buffer is empty.
    pub fn is_write_buf_empty(&self) -> bool {
        self.write_buf.is_empty()
    }

    #[inline]
    /// Check if write buffer is full.
    pub fn is_write_buf_full(&self) -> bool {
        self.write_buf.is_full()
    }

    #[inline]
    /// Check if framed object is closed
    pub fn is_closed(&self) -> bool {
        self.flags.contains(Flags::DISCONNECTED)
    }

    #[inline]
    /// Consume the `Frame`, returning `Frame` with different codec.
    pub fn into_framed<U2>(self, codec: U2) -> Framed<T, U2, P> {
        Framed {
            codec,
            io: self.io,
            flags: self.flags,
            pool: self.pool,
            read_buf: self.read_buf,
            write_buf: self.write_buf,
            err: self.err,
        }
    }

    #[inline]
    /// Consume the `Frame`, returning `Frame` with different io.
    pub fn map_io<F, T2>(self, f: F) -> Framed<T2, U, P>
    where
        F: Fn(T) -> T2,
    {
        Framed {
            io: f(self.io),
            codec: self.codec,
            flags: self.flags,
            pool: self.pool,
            read_buf: self.read_buf,
            write_buf: self.write_buf,
            err: self.err,
        }
    }

    #[inline]
    /// Consume the `Frame`, returning `Frame` with different codec.
    pub fn map_codec<F, U2>(self, f: F) -> Framed<T, U2, P>
    where
        F: Fn(U) -> U2,
    {
        Framed {
            io: self.io,
            codec: f(self.codec),
            flags: self.flags,
            pool: self.pool,
            read_buf: self.read_buf,
            write_buf: self.write_buf,
            err: self.err,
        }
    }

    #[inline]
    /// Consumes the `Frame`, returning its underlying I/O stream, the buffer
    /// with unprocessed data, and the codec.
    ///
    /// Note that care should be taken to not tamper with the underlying stream
    /// of data coming in as it may corrupt the stream of frames otherwise
    /// being worked with.
    pub fn into_parts(self) -> FramedParts<T, U, P> {
        FramedParts {
            io: self.io,
            codec: self.codec,
            flags: self.flags,
            pool: self.pool,
            read_buf: self.read_buf,
            write_buf: self.write_buf,
            err: self.err,
        }
    }
}

impl<T, U, P> Framed<T, U, P>
where
    T: AsyncWrite + Unpin,
    U: Encoder<P>,
    P: BufferPool,
{
    /// Serialize item and Write to the inner buffer
    pub async fn write(
        &mut self,
        item: <U as Encoder<P>>::Item,
    ) -> Result<(), <U as Encoder<P>>::Error> {
        self.codec.encode(item, &self.pool, &mut self.write_buf).await?;
        Ok(())
    }

    #[inline]
    /// Check if framed is able to write more data.
    ///
    /// `Framed` object considers ready if there is free space in write buffer.
    pub fn is_write_ready(&self) -> bool {
        !self.write_buf.is_full()
    }

    /// Flush write buffer to underlying I/O stream.
    pub fn flush(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        log::trace!("flushing framed transport");

        let mut written = 0;

        loop {
            let poll =
                if self.io.is_write_vectored() {
                    let mut io_slices = [IoSlice::new(&[]); NUM_VECTORED_IO_SLICES];

                    let num_io_slices = self.write_buf.chunks_vectored(&mut io_slices[..]);
                    if num_io_slices == 0 {
                        break;
                    }
                    let io_slices = &io_slices[..num_io_slices];

                    Pin::new(&mut self.io).poll_write_vectored(cx, io_slices)
                }
                else {
                    let chunk = self.write_buf.chunk();
                    if chunk.is_empty() {
                        break;
                    }

                    Pin::new(&mut self.io).poll_write(cx, chunk)
                };

            match poll {
                Poll::Pending => break,
                Poll::Ready(Ok(n)) => {
                    if n == 0 {
                        log::trace!(
                            "Disconnected during flush, written {}",
                            written
                        );
                        self.flags.insert(Flags::DISCONNECTED);
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "failed to write frame to transport",
                        )));
                    } else {
                        log::trace!("flushed {} bytes ({} more since last time)", written, n);
                        written += n;
                        // remove written data
                        self.write_buf.advance(n);
                    }
                }
                Poll::Ready(Err(e)) => {
                    log::trace!("Error during flush: {}", e);
                    self.flags.insert(Flags::DISCONNECTED);
                    return Poll::Ready(Err(e));
                }
            }
        }

        // flush
        ready!(Pin::new(&mut self.io).poll_flush(cx))?;

        if self.write_buf.is_empty() {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }
}

impl<T, U> Framed<T, U>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    #[inline]
    /// Flush write buffer and shutdown underlying I/O stream.
    ///
    /// Close method shutdown write side of a io object and
    /// then reads until disconnect or error, high level code must use
    /// timeout for close operation.
    pub fn close(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        if !self.flags.contains(Flags::DISCONNECTED) {
            // flush write buffer
            ready!(Pin::new(&mut self.io).poll_flush(cx))?;

            if !self.flags.contains(Flags::SHUTDOWN) {
                // shutdown WRITE side
                ready!(Pin::new(&mut self.io).poll_shutdown(cx)).map_err(|e| {
                    self.flags.insert(Flags::DISCONNECTED);
                    e
                })?;
                self.flags.insert(Flags::SHUTDOWN);
            }

            // read until 0 or err
            let mut buf = [0u8; 512];
            loop {
                let mut read_buf = tokio::io::ReadBuf::new(&mut buf);
                match ready!(Pin::new(&mut self.io).poll_read(cx, &mut read_buf)) {
                    Err(_) | Ok(_) if read_buf.filled().is_empty() => {
                        break;
                    }
                    _ => (),
                }
            }
            self.flags.insert(Flags::DISCONNECTED);
        }
        log::trace!("framed transport flushed and closed");
        Poll::Ready(Ok(()))
    }
}

pub type ItemType<U, P = BytesPool> =
    Result<<U as Decoder<P>>::Item, Either<<U as Decoder<P>>::Error, io::Error>>;

impl<T, U> Framed<T, U>
where
    T: AsyncRead + Unpin,
    U: Decoder,
{
    /// Try to read underlying I/O stream and decode item.
    pub async fn next_item(&mut self) -> Option<ItemType<U>> {
        loop {
            // Repeatedly call `decode` or `decode_eof` as long as it is
            // "readable". Readable is defined as not having returned `None`. If
            // the upstream has returned EOF, and the decoder is no longer
            // readable, it can be assumed that the decoder will never become
            // readable again, at which point the stream is terminated.

            if self.flags.contains(Flags::READABLE) {
                if self.flags.contains(Flags::EOF) {
                    return match self.codec.decode_eof(&mut self.read_buf) {
                        Ok(None) => {
                            if let Some(err) = self.err.take() {
                                Some(Err(Either::Right(err)))
                            } else if !self.read_buf.is_empty() {
                                Some(Err(Either::Right(io::Error::new(
                                    io::ErrorKind::Other,
                                    "bytes remaining on stream",
                                ))))
                            } else {
                                None
                            }
                        },
                        Ok(Some(item)) => Some(Ok(item)),
                        Err(err) => Some(Err(Either::Left(err))),
                    };
                }

                log::trace!("attempting to decode a frame");

                match self.codec.decode(&mut self.read_buf) {
                    Ok(Some(frame)) => {
                        log::trace!("frame decoded from buffer");
                        return Some(Ok(frame));
                    }
                    Err(e) => return Some(Err(Either::Left(e))),
                    Ok(None) => (), // Need more data
                }

                self.flags.remove(Flags::READABLE);
            }

            debug_assert!(!self.flags.contains(Flags::EOF));

            // read all data from socket
            let mut updated = false;
            loop {
                // Otherwise, try to read more data and try again.
                match crate::read_buf(Pin::new(&mut self.io), &mut self.read_buf, &self.pool, HW).await {
                    Ok(0) => {
                        self.flags.insert(Flags::EOF | Flags::READABLE);
                        break;
                    }
                    Ok(_) => {
                        updated = true;
                    }
                    Err(e) => {
                        if updated {
                            self.err = Some(e);
                            self.flags.insert(Flags::EOF | Flags::READABLE);
                            break;
                        } else {
                            return Some(Err(Either::Right(e)));
                        }
                    }
                }
            }
        }
    }
}

impl<T, U, P> fmt::Debug for Framed<T, U, P>
where
    T: fmt::Debug,
    U: fmt::Debug,
    P: BufferPool,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Framed")
            .field("io", &self.io)
            .field("codec", &self.codec)
            .finish()
    }
}

/// `FramedParts` contains an export of the data of a Framed transport.
/// It can be used to construct a new `Framed` with a different codec.
/// It contains all current buffers and the inner transport.
pub struct FramedParts<T, U, P = BytesPool> where P: BufferPool {
    /// The inner transport used to read bytes to and write bytes to
    pub io: T,

    /// The codec
    pub codec: U,

    /// The buffer pool.
    pub pool: P,

    /// The buffer with read but unprocessed data.
    pub read_buf: <P as BufferSource>::Owned,

    /// A buffer with unprocessed data which are not written yet.
    pub write_buf: P::Accumulator,

    flags: Flags,
    err: Option<io::Error>,
}

impl<T, U, P> FramedParts<T, U, P> where P: BufferPool {
    /// Create a new, default, `FramedParts`
    pub async fn new(io: T, codec: U, pool: P) -> FramedParts<T, U, P> {
        let read_buf = pool.take(HW).await;
        FramedParts::with_read_buf(io, codec, pool, read_buf)
    }

    /// Create a new `FramedParts` with read buffer
    pub fn with_read_buf(io: T, codec: U, pool: P, read_buf: <P as BufferSource>::Owned) -> FramedParts<T, U, P> {
        let write_buf = P::Accumulator::with_capacity(HW);
        FramedParts {
            io,
            codec,
            pool,
            read_buf,
            write_buf,
            flags: Flags::empty(),
            err: None,
        }
    }
}

impl<T, U, P> fmt::Debug for FramedParts<T, U, P>
where
    T: fmt::Debug,
    U: fmt::Debug,
    P: BufferPool,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FramedParts")
            .field("io", &self.io)
            .field("codec", &self.codec)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use futures::{future::lazy, Sink};
    use ntex::testing::Io;
    use ntex_bytes::Bytes;

    use super::*;
    use crate::BytesCodec;

    #[ntex::test]
    async fn test_basics() {
        let (_, server) = Io::create();
        let mut server = Framed::new(server, BytesCodec);
        server.get_codec_mut();
        server.get_ref();
        server.get_mut();

        let parts = server.into_parts();
        let server = Framed::from_parts(FramedParts::new(parts.io, parts.codec));
        assert!(format!("{:?}", server).contains("Framed"));
    }

    #[ntex::test]
    async fn test_sink() {
        let (client, server) = Io::create();
        client.remote_buffer_cap(1024);
        let mut server = Framed::new(server, BytesCodec);

        assert!(lazy(|cx| Pin::new(&mut server).poll_ready(cx))
            .await
            .is_ready());

        let data = Bytes::from_static(b"GET /test HTTP/1.1\r\n\r\n");
        Pin::new(&mut server).start_send(data).unwrap();
        assert_eq!(client.read_any(), b"".as_ref());
        assert_eq!(server.read_buf(), b"".as_ref());
        assert_eq!(server.write_buf(), b"GET /test HTTP/1.1\r\n\r\n".as_ref());

        assert!(lazy(|cx| Pin::new(&mut server).poll_flush(cx))
            .await
            .is_ready());
        assert_eq!(client.read_any(), b"GET /test HTTP/1.1\r\n\r\n".as_ref());

        assert!(lazy(|cx| Pin::new(&mut server).poll_close(cx))
            .await
            .is_pending());
        client.close().await;
        assert!(lazy(|cx| Pin::new(&mut server).poll_close(cx))
            .await
            .is_ready());
        assert!(client.is_closed());
    }

    #[ntex::test]
    async fn test_write_pending() {
        let (client, server) = Io::create();
        let mut server = Framed::new(server, BytesCodec);

        assert!(lazy(|cx| Pin::new(&mut server).poll_ready(cx))
            .await
            .is_ready());
        let data = Bytes::from_static(b"GET /test HTTP/1.1\r\n\r\n");
        Pin::new(&mut server).start_send(data).unwrap();

        client.remote_buffer_cap(3);
        assert!(lazy(|cx| Pin::new(&mut server).poll_flush(cx))
            .await
            .is_pending());
        assert_eq!(client.read_any(), b"GET".as_ref());

        client.remote_buffer_cap(1024);
        assert!(lazy(|cx| Pin::new(&mut server).poll_flush(cx))
            .await
            .is_ready());
        assert_eq!(client.read_any(), b" /test HTTP/1.1\r\n\r\n".as_ref());

        assert!(lazy(|cx| Pin::new(&mut server).poll_close(cx))
            .await
            .is_pending());
        client.close().await;
        assert!(lazy(|cx| Pin::new(&mut server).poll_close(cx))
            .await
            .is_ready());
        assert!(client.is_closed());
        assert!(server.is_closed());
    }

    #[ntex::test]
    async fn test_read_pending() {
        let (client, server) = Io::create();
        let mut server = Framed::new(server, BytesCodec);

        client.read_pending();
        assert!(lazy(|cx| Pin::new(&mut server).next_item(cx))
            .await
            .is_pending());

        client.write(b"GET /test HTTP/1.1\r\n\r\n");
        client.close().await;

        let item = lazy(|cx| Pin::new(&mut server).next_item(cx))
            .await
            .map(|i| i.unwrap().unwrap().freeze());
        assert_eq!(
            item,
            Poll::Ready(Bytes::from_static(b"GET /test HTTP/1.1\r\n\r\n"))
        );
        let item = lazy(|cx| Pin::new(&mut server).next_item(cx))
            .await
            .map(|i| i.is_none());
        assert_eq!(item, Poll::Ready(true));
    }

    #[ntex::test]
    async fn test_read_error() {
        let (client, server) = Io::create();
        let mut server = Framed::new(server, BytesCodec);

        client.read_pending();
        assert!(lazy(|cx| Pin::new(&mut server).next_item(cx))
            .await
            .is_pending());

        client.write(b"GET /test HTTP/1.1\r\n\r\n");
        client.read_error(io::Error::new(io::ErrorKind::Other, "error"));

        let item = lazy(|cx| Pin::new(&mut server).next_item(cx))
            .await
            .map(|i| i.unwrap().unwrap().freeze());
        assert_eq!(
            item,
            Poll::Ready(Bytes::from_static(b"GET /test HTTP/1.1\r\n\r\n"))
        );
        assert_eq!(
            lazy(|cx| Pin::new(&mut server).next_item(cx))
                .await
                .map(|i| i.unwrap().is_err()),
            Poll::Ready(true)
        );
    }
}
