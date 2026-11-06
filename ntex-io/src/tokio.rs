//! Tokio [`AsyncRead`] and [`AsyncWrite`] support for [`Io`] and [`IoBoxed`].
use std::task::{Context, Poll, ready};
use std::{cmp, io, pin::Pin};

use tok_io::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::{Io, IoBoxed};

fn poll_read<F>(io: &Io<F>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
    if buf.remaining() == 0 {
        return Poll::Ready(Ok(()));
    }

    loop {
        let len = io.with_read_dst(|src| {
            let len = cmp::min(src.len(), buf.remaining());
            buf.put_slice(&src[..len]);
            src.advance_to(len);
            len
        });
        if len != 0 {
            return Poll::Ready(Ok(()));
        }

        // `Some` reports new input, drain it on the next iteration. Draining
        // clears the read readiness, so the following poll registers the waker.
        match ready!(io.poll_read_more(cx)) {
            Ok(Some(())) => {}
            Ok(None) => return Poll::Ready(Ok(())),
            Err(e) => return Poll::Ready(Err(e)),
        }
    }
}

fn poll_write<F>(io: &Io<F>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
    if buf.is_empty() {
        return Poll::Ready(Ok(0));
    }
    // Wait while write backpressure is active before accepting more output.
    ready!(io.poll_flush(cx, false))?;
    io.encode_slice(buf)?;
    Poll::Ready(Ok(buf.len()))
}

/// Reads the connection's application data.
///
/// A read returns buffered input first and waits for the transport only when
/// the read buffer is empty. Zero bytes means the peer closed the connection.
///
/// Reads and writes share one task waker: drive both directions from the same
/// task, for example do not move halves of [`tokio::io::split`](https://docs.rs/tokio/latest/tokio/io/fn.split.html)
/// into separate tasks.
#[cfg_attr(docsrs, doc(cfg(feature = "tokio")))]
impl<F> AsyncRead for Io<F> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        poll_read(Pin::into_ref(self).get_ref(), cx, buf)
    }
}

/// Writes application data through the connection's filter chain.
///
/// A write waits while write backpressure is active and then buffers the
/// whole slice. [`poll_flush`](AsyncWrite::poll_flush) waits until the
/// buffered output reaches the transport, [`poll_shutdown`](AsyncWrite::poll_shutdown)
/// gracefully closes the whole connection, see [`Io::poll_shutdown`].
///
/// Reads and writes share one task waker: drive both directions from the same
/// task.
#[cfg_attr(docsrs, doc(cfg(feature = "tokio")))]
impl<F> AsyncWrite for Io<F> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        poll_write(Pin::into_ref(self).get_ref(), cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::into_ref(self).get_ref().poll_flush(cx, true)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::into_ref(self).get_ref().poll_shutdown(cx)
    }
}

/// Same as the [`AsyncRead`] implementation of [`Io`].
#[cfg_attr(docsrs, doc(cfg(feature = "tokio")))]
impl AsyncRead for IoBoxed {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        poll_read(&Pin::into_ref(self).get_ref().0, cx, buf)
    }
}

/// Same as the [`AsyncWrite`] implementation of [`Io`].
#[cfg_attr(docsrs, doc(cfg(feature = "tokio")))]
impl AsyncWrite for IoBoxed {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        poll_write(&Pin::into_ref(self).get_ref().0, cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::into_ref(self).get_ref().0.poll_flush(cx, true)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::into_ref(self).get_ref().0.poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use std::{future::poll_fn, rc::Rc};

    use ntex_service::cfg::SharedCfg;
    use ntex_util::time::{Millis, sleep};
    use tok_io::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;
    use crate::{cfg::IoConfig, testing::IoTest};

    /// STATEMENT: `Io` reads peer input and writes output through the tokio traits.
    ///
    /// HOW:
    /// - Write peer bytes and read them with `AsyncReadExt::read`, first into a
    ///   short buffer to check that the remainder stays buffered.
    /// - Write with `AsyncWriteExt::write_all` and `AsyncWriteExt::flush` and check the peer receives the bytes.
    #[ntex::test]
    async fn io_reads_and_writes() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        let mut io = Io::new(server, SharedCfg::new("SRV"));

        client.write(b"hello world");
        let mut buf = [0; 5];
        assert_eq!(io.read(&mut buf).await.unwrap(), 5);
        assert_eq!(&buf, b"hello");
        let mut buf = [0; 64];
        assert_eq!(io.read(&mut buf).await.unwrap(), 6);
        assert_eq!(&buf[..6], b" world");

        io.write_all(b"response").await.unwrap();
        AsyncWriteExt::flush(&mut io).await.unwrap();
        assert_eq!(client.read_any(), b"response".as_ref());
    }

    /// STATEMENT: A pending `IoBoxed` read wakes when input arrives and reports EOF as zero bytes.
    ///
    /// HOW:
    /// - Start a read on an `IoBoxed` before any input exists and check it is still pending.
    /// - Write peer bytes from a spawned task and check the read returns them.
    /// - Close the peer and check the next read returns `0`.
    #[ntex::test]
    async fn boxed_read_waits_for_input_and_eof() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        let mut io = IoBoxed::from(Io::new(server, SharedCfg::new("SRV")));

        let mut buf = [0; 16];
        let mut read = Box::pin(io.read(&mut buf));
        let polled = poll_fn(|cx| Poll::Ready(read.as_mut().poll(cx).is_pending())).await;
        assert!(polled);

        let client = Rc::new(client);
        let peer = client.clone();
        ntex_rt::spawn(async move {
            sleep(Millis(20)).await;
            peer.write(b"data");
        });
        assert_eq!(read.await.unwrap(), 4);
        assert_eq!(&buf[..4], b"data");

        client.close().await;
        assert_eq!(io.read(&mut buf).await.unwrap(), 0);
    }

    /// STATEMENT: A write waits while write backpressure is active.
    ///
    /// HOW:
    /// - Configure a small write backpressure size and block the peer.
    /// - Write past the limit and check the next `poll_write` stays pending.
    /// - Unblock the peer and check the write completes and all bytes arrive.
    #[ntex::test]
    async fn write_respects_backpressure() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(0);
        let cfg = SharedCfg::new("SRV").add(IoConfig::default().set_write_backpressure(64));
        let mut io = Io::new(server, cfg);

        io.write_all(&[b'a'; 128]).await.unwrap();
        let pending =
            poll_fn(|cx| Poll::Ready(Pin::new(&mut io).poll_write(cx, b"b").is_pending())).await;
        assert!(pending);

        client.remote_buffer_cap(1024);
        io.write_all(b"b").await.unwrap();
        AsyncWriteExt::flush(&mut io).await.unwrap();
        let data = client.read_any();
        assert_eq!(data.len(), 129);
        assert_eq!(data[128], b'b');
    }

    /// STATEMENT: Tokio shutdown gracefully closes the connection.
    ///
    /// HOW:
    /// - Buffer output, call `AsyncWriteExt::shutdown` and check it completes.
    /// - Check the peer received the output and observed the close.
    #[ntex::test]
    async fn shutdown_closes_connection() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        let mut io = Io::new(server, SharedCfg::new("SRV"));

        io.write_all(b"bye").await.unwrap();
        AsyncWriteExt::shutdown(&mut io).await.unwrap();
        assert_eq!(client.read_any(), b"bye".as_ref());
        assert!(client.is_closed());
        assert!(io.is_closed());
    }
}
