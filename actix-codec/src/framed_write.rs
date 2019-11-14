use std::fmt;
use std::io::{self, Read};
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::BytesMut;
use futures::{ready, Sink, Stream};
use log::trace;
use tokio_codec::{Decoder, Encoder};
use tokio_io::{AsyncRead, AsyncWrite};

use super::framed::Fuse;

/// A `Sink` of frames encoded to an `AsyncWrite`.
pub struct FramedWrite<T, E> {
    inner: FramedWrite2<Fuse<T, E>>,
}

pub struct FramedWrite2<T> {
    inner: T,
    buffer: BytesMut,
    low_watermark: usize,
    high_watermark: usize,
}

impl<T, E> FramedWrite<T, E>
where
    T: AsyncWrite,
    E: Encoder,
{
    /// Creates a new `FramedWrite` with the given `encoder`.
    pub fn new(inner: T, encoder: E, lw: usize, hw: usize) -> FramedWrite<T, E> {
        FramedWrite {
            inner: framed_write2(Fuse(inner, encoder), lw, hw),
        }
    }
}

impl<T, E> FramedWrite<T, E> {
    /// Returns a reference to the underlying I/O stream wrapped by
    /// `FramedWrite`.
    ///
    /// Note that care should be taken to not tamper with the underlying stream
    /// of data coming in as it may corrupt the stream of frames otherwise
    /// being worked with.
    pub fn get_ref(&self) -> &T {
        &self.inner.inner.0
    }

    /// Returns a mutable reference to the underlying I/O stream wrapped by
    /// `FramedWrite`.
    ///
    /// Note that care should be taken to not tamper with the underlying stream
    /// of data coming in as it may corrupt the stream of frames otherwise
    /// being worked with.
    pub fn get_mut(&mut self) -> &mut T {
        &mut self.inner.inner.0
    }

    /// Consumes the `FramedWrite`, returning its underlying I/O stream.
    ///
    /// Note that care should be taken to not tamper with the underlying stream
    /// of data coming in as it may corrupt the stream of frames otherwise
    /// being worked with.
    pub fn into_inner(self) -> T {
        self.inner.inner.0
    }

    /// Returns a reference to the underlying decoder.
    pub fn encoder(&self) -> &E {
        &self.inner.inner.1
    }

    /// Returns a mutable reference to the underlying decoder.
    pub fn encoder_mut(&mut self) -> &mut E {
        &mut self.inner.inner.1
    }

    /// Check if write buffer is full
    pub fn is_full(&self) -> bool {
        self.inner.is_full()
    }

    /// Check if write buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

impl<T, E> FramedWrite<T, E>
where
    E: Encoder,
{
    /// Force send item
    pub fn force_send(&mut self, item: E::Item) -> Result<(), E::Error> {
        self.inner.force_send(item)
    }
}

impl<T, E> Sink<E::Item> for FramedWrite<T, E>
where
    T: AsyncWrite,
    E: Encoder,
{
    type Error = E::Error;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        unsafe { self.map_unchecked_mut(|s| &mut s.inner).poll_ready(cx) }
    }

    fn start_send(self: Pin<&mut Self>, item: <E as Encoder>::Item) -> Result<(), Self::Error> {
        unsafe { self.map_unchecked_mut(|s| &mut s.inner).start_send(item) }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        unsafe { self.map_unchecked_mut(|s| &mut s.inner).poll_flush(cx) }
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        unsafe { self.map_unchecked_mut(|s| &mut s.inner).poll_close(cx) }
    }
}

impl<T, D> Stream for FramedWrite<T, D>
where
    T: Stream,
{
    type Item = T::Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        unsafe {
            self.map_unchecked_mut(|s| &mut s.inner.inner.0)
                .poll_next(cx)
        }
    }
}

impl<T, U> fmt::Debug for FramedWrite<T, U>
where
    T: fmt::Debug,
    U: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("FramedWrite")
            .field("inner", &self.inner.get_ref().0)
            .field("encoder", &self.inner.get_ref().1)
            .field("buffer", &self.inner.buffer)
            .finish()
    }
}

// ===== impl FramedWrite2 =====

pub fn framed_write2<T>(
    inner: T,
    low_watermark: usize,
    high_watermark: usize,
) -> FramedWrite2<T> {
    FramedWrite2 {
        inner,
        low_watermark,
        high_watermark,
        buffer: BytesMut::with_capacity(high_watermark),
    }
}

pub fn framed_write2_with_buffer<T>(
    inner: T,
    mut buffer: BytesMut,
    low_watermark: usize,
    high_watermark: usize,
) -> FramedWrite2<T> {
    if buffer.capacity() < high_watermark {
        let bytes_to_reserve = high_watermark - buffer.capacity();
        buffer.reserve(bytes_to_reserve);
    }
    FramedWrite2 {
        inner,
        buffer,
        low_watermark,
        high_watermark,
    }
}

impl<T> FramedWrite2<T> {
    pub fn get_ref(&self) -> &T {
        &self.inner
    }

    pub fn into_inner(self) -> T {
        self.inner
    }

    pub fn into_parts(self) -> (T, BytesMut, usize, usize) {
        (
            self.inner,
            self.buffer,
            self.low_watermark,
            self.high_watermark,
        )
    }

    pub fn get_mut(&mut self) -> &mut T {
        &mut self.inner
    }

    pub fn is_full(&self) -> bool {
        self.buffer.len() >= self.high_watermark
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }
}

impl<T> FramedWrite2<T>
where
    T: Encoder,
{
    pub fn force_send(&mut self, item: T::Item) -> Result<(), T::Error> {
        let len = self.buffer.len();
        if len < self.low_watermark {
            self.buffer.reserve(self.high_watermark - len)
        }
        self.inner.encode(item, &mut self.buffer)?;
        Ok(())
    }
}

impl<T> Sink<T::Item> for FramedWrite2<T>
where
    T: AsyncWrite + Encoder,
{
    type Error = T::Error;

    fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let len = self.buffer.len();
        if len >= self.high_watermark {
            Poll::Pending
        } else {
            Poll::Ready(Ok(()))
        }
    }

    fn start_send(self: Pin<&mut Self>, item: <T as Encoder>::Item) -> Result<(), Self::Error> {
        let this = unsafe { self.get_unchecked_mut() };

        // Check the buffer capacity
        let len = this.buffer.len();
        if len < this.low_watermark {
            this.buffer.reserve(this.high_watermark - len)
        }

        this.inner.encode(item, &mut this.buffer)?;

        Ok(())
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let this = unsafe { self.get_unchecked_mut() };
        trace!("flushing framed transport");

        while !this.buffer.is_empty() {
            trace!("writing; remaining={}", this.buffer.len());

            let n = ready!(
                unsafe { Pin::new_unchecked(&mut this.inner) }.poll_write(cx, &this.buffer)
            )?;

            if n == 0 {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "failed to \
                     write frame to transport",
                )
                .into()));
            }

            // TODO: Add a way to `bytes` to do this w/o returning the drained
            // data.
            let _ = this.buffer.split_to(n);
        }

        // Try flushing the underlying IO
        ready!(unsafe { Pin::new_unchecked(&mut this.inner) }.poll_flush(cx))?;

        trace!("framed transport flushed");
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let mut this = unsafe { self.get_unchecked_mut() };
        ready!(
            unsafe { Pin::new_unchecked(&mut this).map_unchecked_mut(|s| *s) }.poll_flush(cx)
        )?;
        ready!(unsafe { Pin::new_unchecked(&mut this.inner) }.poll_shutdown(cx))?;

        Poll::Ready(Ok(()))
    }
}

impl<T: Decoder> Decoder for FramedWrite2<T> {
    type Item = T::Item;
    type Error = T::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<T::Item>, T::Error> {
        self.inner.decode(src)
    }

    fn decode_eof(&mut self, src: &mut BytesMut) -> Result<Option<T::Item>, T::Error> {
        self.inner.decode_eof(src)
    }
}

impl<T: Read> Read for FramedWrite2<T> {
    fn read(&mut self, dst: &mut [u8]) -> io::Result<usize> {
        self.inner.read(dst)
    }
}

impl<T: AsyncRead> AsyncRead for FramedWrite2<T> {
    unsafe fn prepare_uninitialized_buffer(&self, buf: &mut [u8]) -> bool {
        self.inner.prepare_uninitialized_buffer(buf)
    }

    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        unsafe { self.map_unchecked_mut(|s| &mut s.inner).poll_read(cx, buf) }
    }
}
