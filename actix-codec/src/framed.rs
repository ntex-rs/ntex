use std::pin::Pin;
use std::task::{Context, Poll};
use std::{fmt, io};

use bytes::{Buf, BytesMut};
use futures_core::{ready, Stream};
use futures_sink::Sink;

use crate::{AsyncRead, AsyncWrite, Decoder, Encoder};

const LW: usize = 1024;
const HW: usize = 8 * 1024;

bitflags::bitflags! {
    struct Flags: u8 {
        const EOF = 0b0001;
        const READABLE = 0b0010;
    }
}

/// A unified `Stream` and `Sink` interface to an underlying I/O object, using
/// the `Encoder` and `Decoder` traits to encode and decode frames.
pub struct Framed<T, U> {
    io: T,
    codec: U,
    flags: Flags,
    read_buf: BytesMut,
    write_buf: BytesMut,
}

impl<T, U> Unpin for Framed<T, U> {}

impl<T, U> Framed<T, U>
where
    T: AsyncRead + AsyncWrite,
    U: Decoder + Encoder,
{
    /// Provides a `Stream` and `Sink` interface for reading and writing to this
    /// `Io` object, using `Decode` and `Encode` to read and write the raw data.
    ///
    /// Raw I/O objects work with byte sequences, but higher-level code usually
    /// wants to batch these into meaningful chunks, called "frames". This
    /// method layers framing on top of an I/O object, by using the `Codec`
    /// traits to handle encoding and decoding of messages frames. Note that
    /// the incoming and outgoing frame types may be distinct.
    ///
    /// This function returns a *single* object that is both `Stream` and
    /// `Sink`; grouping this into a single object is often useful for layering
    /// things like gzip or TLS, which require both read and write access to the
    /// underlying object.
    pub fn new(io: T, codec: U) -> Framed<T, U> {
        Framed {
            io,
            codec,
            flags: Flags::empty(),
            read_buf: BytesMut::with_capacity(HW),
            write_buf: BytesMut::with_capacity(HW),
        }
    }
}

impl<T, U> Framed<T, U> {
    /// Provides a `Stream` and `Sink` interface for reading and writing to this
    /// `Io` object, using `Decode` and `Encode` to read and write the raw data.
    ///
    /// Raw I/O objects work with byte sequences, but higher-level code usually
    /// wants to batch these into meaningful chunks, called "frames". This
    /// method layers framing on top of an I/O object, by using the `Codec`
    /// traits to handle encoding and decoding of messages frames. Note that
    /// the incoming and outgoing frame types may be distinct.
    ///
    /// This function returns a *single* object that is both `Stream` and
    /// `Sink`; grouping this into a single object is often useful for layering
    /// things like gzip or TLS, which require both read and write access to the
    /// underlying object.
    ///
    /// This objects takes a stream and a readbuffer and a writebuffer. These
    /// field can be obtained from an existing `Framed` with the
    /// `into_parts` method.
    pub fn from_parts(parts: FramedParts<T, U>) -> Framed<T, U> {
        Framed {
            io: parts.io,
            codec: parts.codec,
            flags: parts.flags,
            write_buf: parts.write_buf,
            read_buf: parts.read_buf,
        }
    }

    /// Returns a reference to the underlying codec.
    pub fn get_codec(&self) -> &U {
        &self.codec
    }

    /// Returns a mutable reference to the underlying codec.
    pub fn get_codec_mut(&mut self) -> &mut U {
        &mut self.codec
    }

    /// Returns a reference to the underlying I/O stream wrapped by
    /// `Frame`.
    ///
    /// Note that care should be taken to not tamper with the underlying stream
    /// of data coming in as it may corrupt the stream of frames otherwise
    /// being worked with.
    pub fn get_ref(&self) -> &T {
        &self.io
    }

    /// Returns a mutable reference to the underlying I/O stream wrapped by
    /// `Frame`.
    ///
    /// Note that care should be taken to not tamper with the underlying stream
    /// of data coming in as it may corrupt the stream of frames otherwise
    /// being worked with.
    pub fn get_mut(&mut self) -> &mut T {
        &mut self.io
    }

    /// Check if write buffer is empty.
    pub fn is_write_buf_empty(&self) -> bool {
        self.write_buf.is_empty()
    }

    /// Check if write buffer is full.
    pub fn is_write_buf_full(&self) -> bool {
        self.write_buf.len() >= HW
    }

    /// Consume the `Frame`, returning `Frame` with different codec.
    pub fn into_framed<U2>(self, codec: U2) -> Framed<T, U2> {
        Framed {
            codec,
            io: self.io,
            flags: self.flags,
            read_buf: self.read_buf,
            write_buf: self.write_buf,
        }
    }

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
        }
    }

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
        }
    }

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
        }
    }
}

impl<T, U> Framed<T, U> {
    /// Serialize item and Write to the inner buffer
    pub fn write(&mut self, item: <U as Encoder>::Item) -> Result<(), <U as Encoder>::Error>
    where
        T: AsyncWrite,
        U: Encoder,
    {
        let remaining = self.write_buf.capacity() - self.write_buf.len();
        if remaining < LW {
            self.write_buf.reserve(HW - remaining);
        }

        self.codec.encode(item, &mut self.write_buf)?;
        Ok(())
    }

    /// Check if framed is able to write more data.
    ///
    /// `Framed` object considers ready if there is free space in write buffer.
    pub fn is_write_ready(&self) -> bool {
        self.write_buf.len() < HW
    }

    /// Try to read underlying I/O stream and decode item.
    pub fn next_item(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<U::Item, U::Error>>>
    where
        T: AsyncRead,
        U: Decoder,
    {
        loop {
            // Repeatedly call `decode` or `decode_eof` as long as it is
            // "readable". Readable is defined as not having returned `None`. If
            // the upstream has returned EOF, and the decoder is no longer
            // readable, it can be assumed that the decoder will never become
            // readable again, at which point the stream is terminated.

            if self.flags.contains(Flags::READABLE) {
                if self.flags.contains(Flags::EOF) {
                    match self.codec.decode_eof(&mut self.read_buf) {
                        Ok(Some(frame)) => return Poll::Ready(Some(Ok(frame))),
                        Ok(None) => return Poll::Ready(None),
                        Err(e) => return Poll::Ready(Some(Err(e))),
                    }
                }

                log::trace!("attempting to decode a frame");

                match self.codec.decode(&mut self.read_buf) {
                    Ok(Some(frame)) => {
                        log::trace!("frame decoded from buffer");
                        return Poll::Ready(Some(Ok(frame)));
                    }
                    Err(e) => return Poll::Ready(Some(Err(e))),
                    _ => (), // Need more data
                }

                self.flags.remove(Flags::READABLE);
            }

            debug_assert!(!self.flags.contains(Flags::EOF));

            // Otherwise, try to read more data and try again. Make sure we've got room
            let remaining = self.read_buf.capacity() - self.read_buf.len();
            if remaining < LW {
                self.read_buf.reserve(HW - remaining)
            }
            let cnt = match unsafe {
                Pin::new_unchecked(&mut self.io).poll_read_buf(cx, &mut self.read_buf)
            } {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(e)) => return Poll::Ready(Some(Err(e.into()))),
                Poll::Ready(Ok(cnt)) => cnt,
            };

            if cnt == 0 {
                self.flags.insert(Flags::EOF);
            }
            self.flags.insert(Flags::READABLE);
        }
    }

    /// Flush write buffer to underlying I/O stream.
    pub fn flush(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), U::Error>>
    where
        T: AsyncWrite,
        U: Encoder,
    {
        log::trace!("flushing framed transport");

        while !self.write_buf.is_empty() {
            log::trace!("writing; remaining={}", self.write_buf.len());

            let n = ready!(unsafe {
                Pin::new_unchecked(&mut self.io).poll_write(cx, &self.write_buf)
            })?;

            if n == 0 {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "failed to write frame to transport",
                )
                .into()));
            }

            // remove written data
            self.write_buf.advance(n);
        }

        // Try flushing the underlying IO
        ready!(unsafe { Pin::new_unchecked(&mut self.io).poll_flush(cx) })?;

        log::trace!("framed transport flushed");
        Poll::Ready(Ok(()))
    }

    /// Flush write buffer and shutdown underlying I/O stream.
    pub fn close(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), U::Error>>
    where
        T: AsyncWrite,
        U: Encoder,
    {
        unsafe {
            ready!(Pin::new_unchecked(&mut self.io).poll_flush(cx))?;
            ready!(Pin::new_unchecked(&mut self.io).poll_shutdown(cx))?;
        }
        Poll::Ready(Ok(()))
    }
}

impl<T, U> Stream for Framed<T, U>
where
    T: AsyncRead,
    U: Decoder,
{
    type Item = Result<U::Item, U::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.next_item(cx)
    }
}

impl<T, U> Sink<U::Item> for Framed<T, U>
where
    T: AsyncWrite,
    U: Encoder,
    U::Error: From<io::Error>,
{
    type Error = U::Error;

    fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if self.is_write_ready() {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }

    fn start_send(
        mut self: Pin<&mut Self>,
        item: <U as Encoder>::Item,
    ) -> Result<(), Self::Error> {
        self.write(item)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        self.flush(cx)
    }

    fn poll_close(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        self.close(cx)
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
}

impl<T, U> FramedParts<T, U> {
    /// Create a new, default, `FramedParts`
    pub fn new(io: T, codec: U) -> FramedParts<T, U> {
        FramedParts {
            io,
            codec,
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
            flags: Flags::empty(),
            write_buf: BytesMut::new(),
        }
    }
}
