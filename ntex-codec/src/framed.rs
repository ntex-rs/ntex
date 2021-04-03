use std::pin::Pin;
use std::task::{Context, Poll};
use std::{fmt, io};

use bytes::{Buf, BytesMut};
use ntex_util::{future::Either, ready, Sink, Stream};

use crate::{AsyncRead, AsyncWrite, Decoder, Encoder};

const LW: usize = 1024;
const HW: usize = 8 * 1024;

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
pub struct Framed<T, U> {
    io: T,
    codec: U,
    flags: Flags,
    read_buf: BytesMut,
    write_buf: BytesMut,
    err: Option<io::Error>,
}

impl<T, U> Framed<T, U>
where
    T: AsyncRead + AsyncWrite,
    U: Decoder + Encoder,
{
    #[inline]
    /// Provides an interface for reading and writing to
    /// `Io` object, using `Decode` and `Encode` traits of codec.
    ///
    /// Raw I/O objects work with byte sequences, but higher-level code usually
    /// wants to batch these into meaningful chunks, called "frames". This
    /// method layers framing on top of an I/O object, by using the `Codec`
    /// traits to handle encoding and decoding of messages frames. Note that
    /// the incoming and outgoing frame types may be distinct.
    pub fn new(io: T, codec: U) -> Framed<T, U> {
        Framed {
            io,
            codec,
            err: None,
            flags: Flags::empty(),
            read_buf: BytesMut::with_capacity(HW),
            write_buf: BytesMut::with_capacity(HW),
        }
    }
}

impl<T, U> Framed<T, U> {
    #[inline]
    /// Construct `Framed` object `parts`.
    pub fn from_parts(parts: FramedParts<T, U>) -> Framed<T, U> {
        Framed {
            io: parts.io,
            codec: parts.codec,
            flags: parts.flags,
            write_buf: parts.write_buf,
            read_buf: parts.read_buf,
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
    pub fn read_buf(&mut self) -> &mut BytesMut {
        &mut self.read_buf
    }

    #[inline]
    /// Get write buffer.
    pub fn write_buf(&mut self) -> &mut BytesMut {
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
        self.write_buf.len() >= HW
    }

    #[inline]
    /// Check if framed object is closed
    pub fn is_closed(&self) -> bool {
        self.flags.contains(Flags::DISCONNECTED)
    }

    #[inline]
    /// Consume the `Frame`, returning `Frame` with different codec.
    pub fn into_framed<U2>(self, codec: U2) -> Framed<T, U2> {
        Framed {
            codec,
            io: self.io,
            flags: self.flags,
            read_buf: self.read_buf,
            write_buf: self.write_buf,
            err: self.err,
        }
    }

    #[inline]
    /// Consume the `Frame`, returning `Frame` with different io.
    pub fn map_io<F, T2>(self, f: F) -> Framed<T2, U>
    where
        F: Fn(T) -> T2,
    {
        Framed {
            io: f(self.io),
            codec: self.codec,
            flags: self.flags,
            read_buf: self.read_buf,
            write_buf: self.write_buf,
            err: self.err,
        }
    }

    #[inline]
    /// Consume the `Frame`, returning `Frame` with different codec.
    pub fn map_codec<F, U2>(self, f: F) -> Framed<T, U2>
    where
        F: Fn(U) -> U2,
    {
        Framed {
            io: self.io,
            codec: f(self.codec),
            flags: self.flags,
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
    pub fn into_parts(self) -> FramedParts<T, U> {
        FramedParts {
            io: self.io,
            codec: self.codec,
            flags: self.flags,
            read_buf: self.read_buf,
            write_buf: self.write_buf,
            err: self.err,
        }
    }
}

impl<T, U> Framed<T, U>
where
    T: AsyncWrite + Unpin,
    U: Encoder,
{
    #[inline]
    /// Serialize item and Write to the inner buffer
    pub fn write(
        &mut self,
        item: <U as Encoder>::Item,
    ) -> Result<(), <U as Encoder>::Error> {
        let remaining = self.write_buf.capacity() - self.write_buf.len();
        if remaining < LW {
            self.write_buf.reserve(HW - remaining);
        }

        self.codec.encode(item, &mut self.write_buf)?;
        Ok(())
    }

    #[inline]
    /// Check if framed is able to write more data.
    ///
    /// `Framed` object considers ready if there is free space in write buffer.
    pub fn is_write_ready(&self) -> bool {
        self.write_buf.len() < HW
    }

    /// Flush write buffer to underlying I/O stream.
    pub fn flush(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        log::trace!("flushing framed transport");

        let len = self.write_buf.len();
        if len != 0 {
            let mut written = 0;
            while written < len {
                match Pin::new(&mut self.io).poll_write(cx, &self.write_buf[written..]) {
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
                            written += n
                        }
                    }
                    Poll::Ready(Err(e)) => {
                        log::trace!("Error during flush: {}", e);
                        self.flags.insert(Flags::DISCONNECTED);
                        return Poll::Ready(Err(e));
                    }
                }
            }
            log::trace!("flushed {} bytes", written);

            // remove written data
            if written == len {
                self.write_buf.clear()
            } else {
                self.write_buf.advance(written);
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
            let mut read_buf = tokio::io::ReadBuf::new(&mut buf);
            loop {
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

pub type ItemType<U> =
    Result<<U as Decoder>::Item, Either<<U as Decoder>::Error, io::Error>>;

impl<T, U> Framed<T, U>
where
    T: AsyncRead + Unpin,
    U: Decoder,
{
    /// Try to read underlying I/O stream and decode item.
    pub fn next_item(&mut self, cx: &mut Context<'_>) -> Poll<Option<ItemType<U>>> {
        let mut done_read = false;

        loop {
            // Repeatedly call `decode` or `decode_eof` as long as it is
            // "readable". Readable is defined as not having returned `None`. If
            // the upstream has returned EOF, and the decoder is no longer
            // readable, it can be assumed that the decoder will never become
            // readable again, at which point the stream is terminated.

            if self.flags.contains(Flags::READABLE) {
                if self.flags.contains(Flags::EOF) {
                    return match self.codec.decode_eof(&mut self.read_buf) {
                        Ok(Some(frame)) => Poll::Ready(Some(Ok(frame))),
                        Ok(None) => {
                            if let Some(err) = self.err.take() {
                                Poll::Ready(Some(Err(Either::Right(err))))
                            } else if !self.read_buf.is_empty() {
                                Poll::Ready(Some(Err(Either::Right(io::Error::new(
                                    io::ErrorKind::Other,
                                    "bytes remaining on stream",
                                )))))
                            } else {
                                Poll::Ready(None)
                            }
                        }
                        Err(e) => return Poll::Ready(Some(Err(Either::Left(e)))),
                    };
                }

                log::trace!("attempting to decode a frame");

                match self.codec.decode(&mut self.read_buf) {
                    Ok(Some(frame)) => {
                        log::trace!("frame decoded from buffer");
                        return Poll::Ready(Some(Ok(frame)));
                    }
                    Err(e) => return Poll::Ready(Some(Err(Either::Left(e)))),
                    _ => (), // Need more data
                }

                self.flags.remove(Flags::READABLE);
                if done_read {
                    return Poll::Pending;
                }
            }

            debug_assert!(!self.flags.contains(Flags::EOF));

            // read all data from socket
            let mut updated = false;
            loop {
                // Otherwise, try to read more data and try again. Make sure we've got room
                let remaining = self.read_buf.capacity() - self.read_buf.len();
                if remaining < LW {
                    self.read_buf.reserve(HW - remaining)
                }
                match crate::poll_read_buf(
                    Pin::new(&mut self.io),
                    cx,
                    &mut self.read_buf,
                ) {
                    Poll::Pending => {
                        if updated {
                            done_read = true;
                            self.flags.insert(Flags::READABLE);
                            break;
                        } else {
                            return Poll::Pending;
                        }
                    }
                    Poll::Ready(Ok(n)) => {
                        if n == 0 {
                            self.flags.insert(Flags::EOF | Flags::READABLE);
                            if updated {
                                done_read = true;
                            }
                            break;
                        } else {
                            updated = true;
                        }
                    }
                    Poll::Ready(Err(e)) => {
                        if updated {
                            done_read = true;
                            self.err = Some(e);
                            self.flags.insert(Flags::EOF | Flags::READABLE);
                            break;
                        } else {
                            return Poll::Ready(Some(Err(Either::Right(e))));
                        }
                    }
                }
            }
        }
    }
}

impl<T, U> Stream for Framed<T, U>
where
    T: AsyncRead + Unpin,
    U: Decoder + Unpin,
{
    type Item = Result<U::Item, Either<U::Error, io::Error>>;

    #[inline]
    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        self.next_item(cx)
    }
}

impl<T, U> Sink<U::Item> for Framed<T, U>
where
    T: AsyncRead + AsyncWrite + Unpin,
    U: Encoder + Unpin,
{
    type Error = Either<U::Error, io::Error>;

    #[inline]
    fn poll_ready(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        if self.is_write_ready() {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }

    #[inline]
    fn start_send(
        mut self: Pin<&mut Self>,
        item: <U as Encoder>::Item,
    ) -> Result<(), Self::Error> {
        self.write(item).map_err(Either::Left)
    }

    #[inline]
    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        self.flush(cx).map_err(Either::Right)
    }

    #[inline]
    fn poll_close(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        self.close(cx).map_err(Either::Right)
    }
}

impl<T, U> fmt::Debug for Framed<T, U>
where
    T: fmt::Debug,
    U: fmt::Debug,
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
#[derive(Debug)]
pub struct FramedParts<T, U> {
    /// The inner transport used to read bytes to and write bytes to
    pub io: T,

    /// The codec
    pub codec: U,

    /// The buffer with read but unprocessed data.
    pub read_buf: BytesMut,

    /// A buffer with unprocessed data which are not written yet.
    pub write_buf: BytesMut,

    flags: Flags,
    err: Option<io::Error>,
}

impl<T, U> FramedParts<T, U> {
    /// Create a new, default, `FramedParts`
    pub fn new(io: T, codec: U) -> FramedParts<T, U> {
        FramedParts {
            io,
            codec,
            err: None,
            flags: Flags::empty(),
            read_buf: BytesMut::new(),
            write_buf: BytesMut::new(),
        }
    }

    /// Create a new `FramedParts` with read buffer
    pub fn with_read_buf(io: T, codec: U, read_buf: BytesMut) -> FramedParts<T, U> {
        FramedParts {
            io,
            codec,
            read_buf,
            err: None,
            flags: Flags::empty(),
            write_buf: BytesMut::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use futures::future::lazy;
    use futures::Sink;
    use ntex::testing::Io;

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
