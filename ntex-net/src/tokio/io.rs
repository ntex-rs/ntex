use std::task::{Context, Poll, ready};
use std::{any, cmp, future::poll_fn, io, pin::Pin, ptr, rc::Rc};

use ntex_bytes::{BufMut, BytePages};
use ntex_io::{Filter, Handle, Io, IoBoxed, IoContext, IoStream, IoTaskStatus, Readiness, types};
use tok_io::io::{AsyncRead, AsyncWrite, ReadBuf};
use tok_io::net::TcpStream;

impl IoStream for super::TcpStream {
    fn start(self, ctx: IoContext) -> Box<dyn Handle> {
        let io = Rc::new(self.0);
        tok_io::task::spawn_local(run_rd(io.clone(), ctx.clone()));
        tok_io::task::spawn_local(run_wrt(io.clone(), ctx));
        Box::new(HandleWrapper(io))
    }
}

#[cfg(unix)]
impl IoStream for super::UnixStream {
    fn start(self, ctx: IoContext) -> Box<dyn Handle> {
        let io = Rc::new(self.0);
        tok_io::task::spawn_local(run_rd(io.clone(), ctx.clone()));
        tok_io::task::spawn_local(run_wrt(io.clone(), ctx));
        Box::new(HandleWrapperUnix(io))
    }
}

trait Stream: AsyncRead + AsyncWrite + Unpin {
    fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>>;

    fn poll_write_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>>;

    /// Closes both directions gracefully, after draining the receive queue.
    fn terminate(&self) -> io::Result<()>;

    /// Arranges for the socket to be reset instead of closed gracefully.
    fn abort(&self);

    fn try_read(&self, buf: &mut [u8]) -> io::Result<usize>;

    fn try_write(&self, buf: &[u8]) -> io::Result<usize>;

    fn try_write_vectored(&self, buf: &[io::IoSlice<'_>]) -> io::Result<usize>;
}

impl Stream for TcpStream {
    fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        TcpStream::poll_read_ready(self, cx)
    }

    fn poll_write_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        TcpStream::poll_write_ready(self, cx)
    }

    fn terminate(&self) -> io::Result<()> {
        let sock = socket2::SockRef::from(self);
        crate::helpers::drain_socket(&sock);
        crate::helpers::shutdown_result(sock.shutdown(std::net::Shutdown::Both))
    }

    fn abort(&self) {
        crate::helpers::abort_socket(&socket2::SockRef::from(self));
    }

    fn try_read(&self, buf: &mut [u8]) -> io::Result<usize> {
        TcpStream::try_read(self, buf)
    }

    fn try_write(&self, buf: &[u8]) -> io::Result<usize> {
        TcpStream::try_write(self, buf)
    }

    fn try_write_vectored(&self, buf: &[io::IoSlice<'_>]) -> io::Result<usize> {
        TcpStream::try_write_vectored(self, buf)
    }
}

#[cfg(unix)]
impl Stream for tok_io::net::UnixStream {
    fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        tok_io::net::UnixStream::poll_read_ready(self, cx)
    }

    fn poll_write_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        tok_io::net::UnixStream::poll_write_ready(self, cx)
    }

    fn terminate(&self) -> io::Result<()> {
        let sock = socket2::SockRef::from(self);
        crate::helpers::drain_socket(&sock);
        crate::helpers::shutdown_result(sock.shutdown(std::net::Shutdown::Both))
    }

    fn abort(&self) {
        crate::helpers::abort_socket(&socket2::SockRef::from(self));
    }

    fn try_read(&self, buf: &mut [u8]) -> io::Result<usize> {
        tok_io::net::UnixStream::try_read(self, buf)
    }

    fn try_write(&self, buf: &[u8]) -> io::Result<usize> {
        tok_io::net::UnixStream::try_write(self, buf)
    }

    fn try_write_vectored(&self, buf: &[io::IoSlice<'_>]) -> io::Result<usize> {
        tok_io::net::UnixStream::try_write_vectored(self, buf)
    }
}

struct HandleWrapper(Rc<TcpStream>);

impl Handle for HandleWrapper {
    fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>> {
        if id == any::TypeId::of::<types::PeerAddr>() {
            let result = self.0.peer_addr();
            if let Ok(addr) = result {
                return Some(Box::new(types::PeerAddr(addr)));
            }
        }
        None
    }

    fn write(&self, ctx: &IoContext) {
        let _ = write(self.0.as_ref(), ctx, true);
    }
}

#[cfg(unix)]
struct HandleWrapperUnix(Rc<tok_io::net::UnixStream>);

#[cfg(unix)]
impl Handle for HandleWrapperUnix {
    fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>> {
        None
    }

    fn write(&self, ctx: &IoContext) {
        let _ = write(self.0.as_ref(), ctx, true);
    }
}

async fn run_rd<T>(io: Rc<T>, ctx: IoContext)
where
    T: Stream + Unpin,
{
    let st = poll_fn(|cx| {
        'outer: loop {
            let ctx_state = ctx.poll_read_ready(cx);
            #[cfg(feature = "trace")]
            log::trace!(
                "{}: Read task, ctx:{ctx_state:?} flags:{:?}",
                ctx.tag(),
                ctx.flags()
            );
            return match ready!(ctx_state) {
                Readiness::Ready => {
                    let io_state = io.poll_read_ready(cx);
                    #[cfg(feature = "trace")]
                    log::trace!("{}: Io read ready: {io_state:?}", ctx.tag());

                    match ready!(io_state) {
                        Ok(()) => 'inner: loop {
                            return match read(io.as_ref(), &ctx) {
                                Poll::Ready(IoTaskStatus::Io) => continue 'inner,
                                Poll::Ready(IoTaskStatus::Pause) => Poll::Pending,
                                Poll::Ready(IoTaskStatus::Stop) => Poll::Ready(()),
                                Poll::Pending => continue 'outer,
                            };
                        },
                        Err(err) => {
                            ctx.stop(Some(err));
                            Poll::Ready(())
                        }
                    }
                }
                Readiness::Close | Readiness::Terminate => Poll::Ready(()),
            };
        }
    })
    .await;
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum WrtStatus {
    More,
    Pending,
    Terminate,
}

async fn run_wrt<T>(io: Rc<T>, ctx: IoContext)
where
    T: Stream,
{
    let terminate = poll_fn(|cx| {
        let ctx_state = ctx.poll_write_ready(cx);
        #[cfg(feature = "trace")]
        log::trace!(
            "{}: Write task, ctx {ctx_state:?} flags:{:?}",
            ctx.tag(),
            ctx.flags()
        );

        match ready!(ctx_state) {
            Readiness::Ready => loop {
                let io_state = io.poll_write_ready(cx);
                #[cfg(feature = "trace")]
                log::trace!("{}: Io write ready {io_state:?}", ctx.tag());

                return match ready!(io_state) {
                    Ok(()) => match write(io.as_ref(), &ctx, false) {
                        WrtStatus::More => continue,
                        WrtStatus::Pending => Poll::Pending,
                        // the connection has been aborted already
                        WrtStatus::Terminate => Poll::Ready(true),
                    },
                    Err(err) => {
                        ctx.update_write_status(Err(err));
                        Poll::Ready(true)
                    }
                };
            },
            Readiness::Close => Poll::Ready(false),
            Readiness::Terminate => Poll::Ready(true),
        }
    })
    .await;

    log::trace!("{}: Shuting down io", ctx.tag());

    // A force-closed connection is aborted instead of closed gracefully, so
    // that a truncated stream is not terminated by a clean `FIN`.
    let result = if terminate {
        io.abort();
        Ok(())
    } else {
        io.terminate()
    };

    log::trace!("{}: Shutdown complete {result:?}", ctx.tag());
    ctx.stopped(result.err());
}

const MAX_WRITE_SIZE: usize = 256 * 1024;
const MAX_WRITE_ITEMS: usize = 32;

/// Output written per write attempt at most, see [`write`].
const WRITE_BUDGET: usize = 256 * 1024;

/// Writes buffered output until the socket is full or the write budget is
/// spent.
///
/// A single write is limited to `MAX_WRITE_SIZE`, so writes continue while
/// the socket accepts everything, a large buffer does not wait for a task
/// wakeup per chunk. A short write means the socket is full. `WRITE_BUDGET`
/// keeps one connection from delaying the others.
fn write<T>(io: &T, ctx: &IoContext, direct: bool) -> WrtStatus
where
    T: Stream,
{
    let result = ctx.with_write_dst(|dst| {
        let mut total = 0;
        loop {
            let (written, size) = write_pages(io, ctx, dst, direct)?;
            total += written;
            if written == 0 || written < size || total >= WRITE_BUDGET {
                return Ok(total);
            }
        }
    });

    let st = ctx.update_write_status(result);
    let result = match st {
        IoTaskStatus::Stop => WrtStatus::Terminate,
        IoTaskStatus::Pause => WrtStatus::Pending,
        IoTaskStatus::Io => WrtStatus::More,
    };

    #[cfg(feature = "trace")]
    log::trace!(
        "{}: Write status \"{st:?}({result:?})\" flags:{:?}",
        ctx.tag(),
        ctx.flags()
    );
    result
}

/// Writes up to `MAX_WRITE_ITEMS` pages or `MAX_WRITE_SIZE` bytes of `dst`.
///
/// Returns the written bytes and the size of the attempted write, both are
/// zero if `dst` is empty. A write that would block writes zero bytes.
#[cfg_attr(not(feature = "trace"), allow(unused_variables))]
fn write_pages<T>(
    io: &T,
    ctx: &IoContext,
    dst: &mut BytePages,
    direct: bool,
) -> io::Result<(usize, usize)>
where
    T: Stream,
{
    #[cfg(feature = "trace")]
    log::trace!("{}: Try write buf({})", ctx.tag(), dst.len());

    let (result, size) = if dst.num_pages() <= 1 {
        // a single page is written without building the slice array
        let Some(chunk) = dst.iter().next() else {
            return Ok((0, 0));
        };
        (write_io(ctx, io, &[io::IoSlice::new(chunk)]), chunk.len())
    } else {
        let mut bufs = [io::IoSlice::new(&[]); MAX_WRITE_ITEMS];
        let mut num = 0;
        let mut size = 0;
        for chunk in dst.iter() {
            bufs[num] = io::IoSlice::new(chunk);
            num += 1;
            size += chunk.len();
            if num == MAX_WRITE_ITEMS || size >= MAX_WRITE_SIZE {
                break;
            }
        }
        (write_io(ctx, io, &bufs[..num]), size)
    };

    // remove written bytes
    if let Poll::Ready(Ok(written)) = result {
        dst.advance_to(cmp::min(written, size));
    }

    #[cfg(feature = "trace")]
    log::trace!(
        "{}: Io write (direct:{direct}) result:{result:?} buf:{} flags:{:?}",
        ctx.tag(),
        dst.len(),
        ctx.flags()
    );

    match result? {
        Poll::Ready(val) => {
            if val == 0 {
                ctx.stop(None);
            }
            Ok((val, size))
        }
        Poll::Pending => Ok((0, size)),
    }
}

/// Flush write buffer to underlying I/O stream.
fn write_io<T: Stream>(
    ctx: &IoContext,
    io: &T,
    bufs: &[io::IoSlice<'_>],
) -> Poll<io::Result<usize>> {
    let result = if bufs.len() == 1 {
        io.try_write(&bufs[0])
    } else {
        io.try_write_vectored(bufs)
    };
    match result {
        Ok(0) => Poll::Ready(Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "failed to write frame to transport",
        ))),
        Ok(n) => {
            #[cfg(feature = "trace")]
            log::trace!("{}: Flushed {n} bytes from {} pages", ctx.tag(), bufs.len());
            Poll::Ready(Ok(n))
        }
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => Poll::Pending,
        Err(e) => Poll::Ready(Err(e)),
    }
}

fn read<T: Stream + Unpin>(io: &T, ctx: &IoContext) -> Poll<IoTaskStatus> {
    let mut pending = false;

    let result = ctx.with_read_buf(|buf| {
        #[cfg(feature = "trace")]
        log::trace!(
            "{}: Read attempt, buf len({}) cap({})",
            ctx.tag(),
            buf.len(),
            buf.remaining_mut()
        );

        // read data from socket
        let io_res = io.try_read(unsafe { &mut *(ptr::from_mut(buf.chunk_mut()) as *mut [u8]) });

        match io_res {
            Ok(0) => Poll::Ready(Ok(0)),
            Ok(n) => {
                // Safety: This is guaranteed to be the number of initialized
                // bytes due to the invariants provided by `try_read()`.
                unsafe { buf.advance_mut(n) };
                Poll::Ready(Ok(n))
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                pending = true;
                Poll::Pending
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    });

    #[cfg(feature = "trace")]
    log::trace!(
        "{}: Read status \"{result:?}\" pending({pending})",
        ctx.tag()
    );

    if result == IoTaskStatus::Io && pending {
        Poll::Pending
    } else {
        Poll::Ready(result)
    }
}

#[derive(Debug)]
pub struct TokioIoBoxed(IoBoxed);

impl std::ops::Deref for TokioIoBoxed {
    type Target = IoBoxed;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<IoBoxed> for TokioIoBoxed {
    fn from(io: IoBoxed) -> TokioIoBoxed {
        TokioIoBoxed(io)
    }
}

impl<F: Filter> From<Io<F>> for TokioIoBoxed {
    fn from(io: Io<F>) -> TokioIoBoxed {
        TokioIoBoxed(IoBoxed::from(io))
    }
}

impl AsyncRead for TokioIoBoxed {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let len = self.0.with_read_dst(|src| {
            let len = cmp::min(src.len(), buf.remaining());
            buf.put_slice(&src.split_to(len));
            len
        });

        if len == 0 {
            match ready!(self.0.poll_read_more(cx)) {
                Ok(Some(())) => Poll::Pending,
                Err(e) => Poll::Ready(Err(e)),
                Ok(None) => Poll::Ready(Ok(())),
            }
        } else {
            Poll::Ready(Ok(()))
        }
    }
}

impl AsyncWrite for TokioIoBoxed {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.0.encode_slice(buf)?;
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.as_ref().0.poll_flush(cx, false)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.as_ref().0.poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use std::net;

    use ntex_service::cfg::SharedCfg;
    use ntex_util::time::{Millis, timeout};

    use super::*;

    #[ntex::test]
    async fn graceful_shutdown_closes_transport_write_half() {
        let listener = net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        client.set_nonblocking(true).unwrap();
        server.set_nonblocking(true).unwrap();

        let client = tok_io::net::TcpStream::from_std(client).unwrap();
        let io = Io::new(
            super::super::TcpStream(tok_io::net::TcpStream::from_std(server).unwrap()),
            SharedCfg::default(),
        );

        io.close();

        let mut buf = [0; 1];
        let read = timeout(Millis(1000), async {
            loop {
                client.readable().await.unwrap();
                match client.try_read(&mut buf) {
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => (),
                    result => break result,
                }
            }
        })
        .await
        .expect("transport write half was not shut down")
        .unwrap();
        assert_eq!(read, 0);

        // Keep the Io alive until after EOF is observed. Dropping it must not be
        // what closes the peer-facing write half.
        drop(io);
    }

    struct CaptureCtx(std::rc::Rc<std::cell::Cell<Option<IoContext>>>);

    struct NoHandle;

    impl ntex_io::Handle for NoHandle {}

    impl ntex_io::IoStream for CaptureCtx {
        fn start(self, ctx: IoContext) -> Box<dyn ntex_io::Handle> {
            self.0.set(Some(ctx));
            Box::new(NoHandle)
        }
    }

    /// A write continues past one `writev` while the socket accepts
    /// everything, but stops at `WRITE_BUDGET`.
    #[ntex::test]
    async fn write_continues_until_budget() {
        use ntex_bytes::BytePageSize;
        use ntex_io::IoConfig;

        const LARGE: usize = 8 * 1024 * 1024;
        let page = BytePageSize::Size4;
        let size = 2 * WRITE_BUDGET;

        let listener = net::TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (peer, _) = listener.accept().unwrap();
        socket2::SockRef::from(&stream)
            .set_send_buffer_size(size)
            .unwrap();
        socket2::SockRef::from(&peer)
            .set_recv_buffer_size(size)
            .unwrap();
        stream.set_nonblocking(true).unwrap();
        let stream = tok_io::net::TcpStream::from_std(stream).unwrap();
        // `try_write` reports `WouldBlock` until readiness has been observed.
        stream.writable().await.unwrap();

        let slot = std::rc::Rc::new(std::cell::Cell::new(None));
        let io = Io::new(
            CaptureCtx(slot.clone()),
            SharedCfg::new("WRITE").add(IoConfig::default().set_write_size(page)),
        );
        let ctx = slot.take().unwrap();
        // Small pages cap a single `writev` at `MAX_WRITE_ITEMS` pages
        let chunk = [b'x'; 1024];
        for _ in 0..LARGE / chunk.len() {
            io.encode_slice(&chunk).unwrap();
        }

        let _ = write(&stream, &ctx, true);

        let written = LARGE - io.with_write_dst(|b| b.len());
        assert!(
            written > MAX_WRITE_ITEMS * page.capacity(),
            "stopped after one writev: {written}"
        );
        // The last `writev` may start just below the budget
        assert!(
            written < WRITE_BUDGET + MAX_WRITE_SIZE,
            "budget exceeded: {written}"
        );
        drop(peer);
    }

    /// A failed write must hand the pages it took back to the write buffer.
    /// Dropping them loses the output and leaves it counted as in flight,
    /// which nothing will ever report as written.
    #[ntex::test]
    async fn failed_write_returns_pages_to_the_buffer() {
        let listener = net::TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let _peer = listener.accept().unwrap();
        stream.set_nonblocking(true).unwrap();
        // Our own write half is shut, so the next write fails with `EPIPE`.
        stream.shutdown(net::Shutdown::Write).unwrap();
        let stream = tok_io::net::TcpStream::from_std(stream).unwrap();
        // `try_write` reports `WouldBlock` until readiness has been observed.
        stream.writable().await.unwrap();

        let slot = std::rc::Rc::new(std::cell::Cell::new(None));
        let io = Io::new(CaptureCtx(slot.clone()), SharedCfg::default());
        let ctx = slot.take().unwrap();
        io.encode_slice(b"hello").unwrap();

        let status = write(&stream, &ctx, true);

        assert!(matches!(status, WrtStatus::Terminate));
        assert_eq!(
            io.with_write_dst(|b| b.len()),
            5,
            "failed write dropped its pages"
        );
    }
}
