use std::{any, cmp, future::poll_fn, io, mem, task::Poll};

use compio_buf::{BufResult, IoBuf, IoBufMut, SetLen};
use compio_io::{AsyncRead, AsyncWrite};
use ntex_bytes::{BufMut, BytePage, BytePages, BytesMut};
use ntex_io::{Handle, IoContext, IoStream, IoTaskStatus, Readiness, types};
use ntex_util::future::{Either, select};

use super::{TcpStream, UnixStream};

const MAX_WRITE_SIZE: usize = 64 * 1024;
const MAX_WRITE_ITEMS: usize = 16;

impl IoStream for TcpStream {
    fn start(self, ctx: IoContext) -> Box<dyn Handle> {
        compio_runtime::spawn(run(self.0.clone(), ctx)).detach();
        Box::new(HandleWrapper(self.0))
    }
}

impl IoStream for UnixStream {
    fn start(self, ctx: IoContext) -> Box<dyn Handle> {
        compio_runtime::spawn(run(self.0.clone(), ctx)).detach();
        Box::new(HandleUnixWrapper(self.0))
    }
}

struct HandleWrapper(compio_net::TcpStream);

impl Handle for HandleWrapper {
    fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>> {
        if id == any::TypeId::of::<types::PeerAddr>()
            && let Ok(addr) = self.0.peer_addr()
        {
            return Some(Box::new(types::PeerAddr(addr)));
        }
        None
    }
}

#[allow(dead_code)]
struct HandleUnixWrapper(compio_net::UnixStream);

impl Handle for HandleUnixWrapper {
    fn query(&self, _: any::TypeId) -> Option<Box<dyn any::Any>> {
        None
    }
}

struct CompioBuf(BytesMut);

impl IoBuf for CompioBuf {
    #[inline]
    fn as_init(&self) -> &[u8] {
        self.0.as_ref()
    }
}

impl IoBufMut for CompioBuf {
    fn as_uninit(&mut self) -> &mut [mem::MaybeUninit<u8>] {
        self.0.chunk_mut().as_mut()
    }
}

impl SetLen for CompioBuf {
    unsafe fn set_len(&mut self, len: usize) {
        unsafe {
            self.0.advance_mut(len);
        }
    }
}

struct CompioPage(BytePage);

impl IoBuf for CompioPage {
    #[inline]
    fn as_init(&self) -> &[u8] {
        &self.0
    }
}

/// Closes both directions of the connection gracefully.
///
/// `AsyncWrite::shutdown()` only shuts down the write direction, but
/// [`Readiness::Close`] must close the read direction as well. This is not used
/// for [`Readiness::Terminate`], which releases the connection without a
/// graceful close.
trait Terminate {
    /// Closes both directions gracefully, after draining the receive queue.
    fn terminate(&self) -> io::Result<()>;

    /// Arranges for the socket to be reset instead of closed gracefully.
    fn abort(&self);
}

impl Terminate for compio_net::TcpStream {
    fn terminate(&self) -> io::Result<()> {
        let sock = socket2::SockRef::from(self);
        crate::helpers::drain_socket(&sock);
        sock.shutdown(std::net::Shutdown::Both)
    }

    fn abort(&self) {
        crate::helpers::abort_socket(&socket2::SockRef::from(self));
    }
}

impl Terminate for compio_net::UnixStream {
    fn terminate(&self) -> io::Result<()> {
        let sock = socket2::SockRef::from(self);
        crate::helpers::drain_socket(&sock);
        sock.shutdown(std::net::Shutdown::Both)
    }

    fn abort(&self) {
        crate::helpers::abort_socket(&socket2::SockRef::from(self));
    }
}

async fn run<T: AsyncRead + AsyncWrite + Clone + Terminate + Unpin + 'static>(
    io: T,
    ctx: IoContext,
) {
    let wr_io = io.clone();
    let wr_ctx = ctx.clone();
    let wr_task = compio_runtime::spawn(async move {
        write(wr_io, &wr_ctx).await;
        log::debug!("{}: Write task is stopped", wr_ctx.tag());
    });

    read(io, &ctx).await;
    log::debug!("{}: Read task is stopped", ctx.tag());

    if !wr_task.is_finished() {
        let _ = wr_task.await;
    }
}

async fn read<T>(io: T, ctx: &IoContext)
where
    T: AsyncRead + AsyncWrite + Clone + Unpin,
{
    let mut read_fut = Some(Box::pin(read_buf(&io, ctx.take_read_buf())));

    loop {
        if read_ready(ctx).await {
            break;
        }

        match select(read_fut.as_mut().unwrap(), not_read_ready(ctx)).await {
            Either::Left(BufResult(result, cbuf)) => {
                if ctx.release_read_buf(cbuf.0, Poll::Ready(result)) == IoTaskStatus::Stop {
                    break;
                }
                read_fut = Some(Box::pin(read_buf(&io, ctx.take_read_buf())));
            }
            Either::Right(true) => break,
            Either::Right(false) => (),
        }
    }

    log::trace!("{}: Read task shutdown", ctx.tag());
}

async fn read_buf<T>(io: &T, buf: BytesMut) -> BufResult<usize, CompioBuf>
where
    T: AsyncRead + AsyncWrite + Clone,
{
    io.clone().read(CompioBuf(buf)).await
}

async fn read_ready(ctx: &IoContext) -> bool {
    poll_fn(|cx| match ctx.poll_read_ready(cx) {
        Poll::Pending => Poll::Pending,
        Poll::Ready(Readiness::Ready) => Poll::Ready(false),
        Poll::Ready(_) => Poll::Ready(true),
    })
    .await
}

async fn not_read_ready(ctx: &IoContext) -> bool {
    poll_fn(|cx| match ctx.poll_read_ready(cx) {
        Poll::Pending => Poll::Ready(false),
        Poll::Ready(Readiness::Ready) => Poll::Pending,
        Poll::Ready(_) => Poll::Ready(true),
    })
    .await
}

async fn write<T>(mut io: T, ctx: &IoContext)
where
    T: AsyncRead + AsyncWrite + Clone + Terminate,
{
    loop {
        match poll_fn(|cx| ctx.poll_write_ready(cx)).await {
            Readiness::Ready => {
                let bufs = ctx.with_write_dst(build_bufs);

                // The status is not actionable here. `Io` and `Pause` are
                // resolved by the next `poll_write_ready()`, and `Stop` means
                // the connection is already aborted, so that reports `Close`
                // and the transport is torn down there.
                if bufs.is_empty() {
                    ctx.update_write_status(Ok(0));
                } else {
                    write_buf(&mut io, ctx, bufs).await;
                }
            }
            Readiness::Close => {
                ctx.stopped(io.terminate().err());
                break;
            }
            Readiness::Terminate => {
                // The connection was force-closed, so the socket is aborted
                // rather than closed gracefully: the peer sees an RST and
                // cannot mistake a truncated stream for a complete one.
                // Dropping the transport releases the descriptor once both
                // tasks are done with it.
                io.abort();
                ctx.stopped(None);
                break;
            }
        }
    }
}

fn build_bufs(buf: &mut BytePages) -> Vec<CompioPage> {
    let mut bufs = Vec::new();

    let mut num = 0;
    let mut size = 0;
    while let Some(page) = buf.take() {
        num += 1;
        size += page.len();

        bufs.push(CompioPage(page));
        if num == MAX_WRITE_ITEMS || size >= MAX_WRITE_SIZE {
            break;
        }
    }

    bufs
}

async fn write_buf<T>(io: &mut T, ctx: &IoContext, mut bufs: Vec<CompioPage>)
where
    T: AsyncRead + AsyncWrite,
{
    while !bufs.is_empty() {
        let op = async {
            if bufs.len() == 1 {
                let BufResult(result, buf) = io.write(bufs.pop().unwrap()).await;
                (result, vec![buf])
            } else {
                let BufResult(result, bufs) = io.write_vectored(bufs).await;
                (result, bufs)
            }
        };

        // A peer that stops reading keeps the write pending indefinitely, so
        // the connection state is watched as well: the shutdown deadline is
        // only polled from `poll_write_ready()`, and a terminated connection
        // must not wait for the write. Dropping the operation cancels it, the
        // pages are released with it.
        let (result, rest) = match select(op, write_closed(ctx)).await {
            Either::Left(res) => res,
            Either::Right(()) => return,
        };
        bufs = rest;

        let result = match result {
            Ok(0) => Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "failed to write frame to transport",
            )),
            Ok(n) => {
                // remove written pages
                let mut written = n;
                while !bufs.is_empty() {
                    let page = &mut bufs[0];
                    let len = cmp::min(page.0.len(), written);
                    if page.0.len() != len {
                        page.0.advance_to(len);
                        break;
                    }
                    bufs.remove(0);
                    written -= len;
                    if written == 0 {
                        break;
                    }
                }
                Ok(n)
            }
            Err(e) => Err(e),
        };
        if ctx.update_write_status(result) == IoTaskStatus::Stop {
            // Pages still held here are counted as in-flight output, hand
            // back whatever did not reach the peer.
            return_pages(ctx, bufs);
            return;
        }
    }
}

/// Resolves once the connection no longer accepts output.
async fn write_closed(ctx: &IoContext) {
    poll_fn(|cx| match ctx.poll_write_ready(cx) {
        Poll::Ready(Readiness::Terminate | Readiness::Close) => Poll::Ready(()),
        _ => Poll::Pending,
    })
    .await;
}

fn return_pages(ctx: &IoContext, mut bufs: Vec<CompioPage>) {
    if !bufs.is_empty() {
        ctx.with_write_dst(|dst| {
            while let Some(page) = bufs.pop() {
                dst.prepend(page.0);
            }
        });
    }
}
