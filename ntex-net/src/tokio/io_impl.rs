use std::task::{Context, Poll, ready};
use std::{any, cell::RefCell, cmp, future::poll_fn, io, mem, pin::Pin, rc::Rc, rc::Weak};

use ntex_bytes::{BufMut, BytesVec};
use ntex_io::{
    Filter, Handle, Io, IoBoxed, IoContext, IoStream, IoTaskStatus, Readiness, types,
};
use ntex_util::time::Millis;
use tok_io::io::{AsyncRead, AsyncWrite, ReadBuf};
use tok_io::net::TcpStream;

impl IoStream for super::TcpStream {
    fn start(self, ctx: IoContext) -> Option<Box<dyn Handle>> {
        let io = Rc::new(RefCell::new(self.0));
        tok_io::task::spawn_local(run(io.clone(), ctx));
        Some(Box::new(HandleWrapper(io)))
    }
}

#[cfg(unix)]
impl IoStream for super::UnixStream {
    fn start(self, ctx: IoContext) -> Option<Box<dyn Handle>> {
        let io = Rc::new(RefCell::new(self.0));
        tok_io::task::spawn_local(run(io.clone(), ctx));
        None
    }
}

struct HandleWrapper(Rc<RefCell<TcpStream>>);

impl Handle for HandleWrapper {
    fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>> {
        if id == any::TypeId::of::<types::PeerAddr>() {
            if let Ok(addr) = self.0.borrow().peer_addr() {
                return Some(Box::new(types::PeerAddr(addr)));
            }
        } else if id == any::TypeId::of::<SocketOptions>() {
            return Some(Box::new(SocketOptions(Rc::downgrade(&self.0))));
        }
        None
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Status {
    Shutdown,
    Terminate,
}

async fn run<T>(io: Rc<RefCell<T>>, ctx: IoContext)
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    let st = poll_fn(|cx| turn(&mut *io.borrow_mut(), &ctx, cx)).await;

    log::trace!("{}: Shuting down io {:?}", ctx.tag(), ctx.is_stopped());
    if !ctx.is_stopped() {
        let flush = st == Status::Shutdown;
        let _ = poll_fn(|cx| {
            if write(&mut *io.borrow_mut(), &ctx, cx) == Poll::Ready(Status::Terminate) {
                Poll::Ready(())
            } else {
                ctx.shutdown(flush, cx)
            }
        })
        .await;
    }

    let _ = poll_fn(|cx| Pin::new(&mut *io.borrow_mut()).poll_shutdown(cx)).await;

    log::trace!("{}: Shutdown complete", ctx.tag());
    if !ctx.is_stopped() {
        ctx.stop(None);
    }
}

fn turn<T>(io: &mut T, ctx: &IoContext, cx: &mut Context<'_>) -> Poll<Status>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    let read = match ctx.poll_read_ready(cx) {
        Poll::Ready(Readiness::Ready) => read(io, ctx, cx),
        Poll::Ready(Readiness::Shutdown) | Poll::Ready(Readiness::Terminate) => {
            Poll::Ready(())
        }
        Poll::Pending => Poll::Pending,
    };

    let write = match ctx.poll_write_ready(cx) {
        Poll::Ready(Readiness::Ready) => write(io, ctx, cx),
        Poll::Ready(Readiness::Shutdown) => Poll::Ready(Status::Shutdown),
        Poll::Ready(Readiness::Terminate) => Poll::Ready(Status::Terminate),
        Poll::Pending => Poll::Pending,
    };

    if read.is_pending() && write.is_pending() {
        Poll::Pending
    } else if write.is_ready() {
        write
    } else {
        Poll::Ready(Status::Terminate)
    }
}

fn write<T>(io: &mut T, ctx: &IoContext, cx: &mut Context<'_>) -> Poll<Status>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    if let Some(mut buf) = ctx.get_write_buf() {
        let result = write_io(io, &mut buf, cx);
        if ctx.release_write_buf(buf, result) == IoTaskStatus::Stop {
            Poll::Ready(Status::Terminate)
        } else {
            Poll::Pending
        }
    } else {
        Poll::Pending
    }
}

fn read<T: AsyncRead + Unpin>(
    io: &mut T,
    ctx: &IoContext,
    cx: &mut Context<'_>,
) -> Poll<()> {
    let mut buf = ctx.get_read_buf();

    // read data from socket
    let mut n = 0;
    loop {
        ctx.resize_read_buf(&mut buf);
        let result = match read_buf(Pin::new(&mut *io), cx, &mut buf) {
            Poll::Pending => {
                if n > 0 {
                    Poll::Ready(Ok(()))
                } else {
                    Poll::Pending
                }
            }
            Poll::Ready(Ok(0)) => Poll::Ready(Err(None)),
            Poll::Ready(Ok(size)) => {
                n += size;
                continue;
            }
            Poll::Ready(Err(err)) => Poll::Ready(Err(Some(err))),
        };

        return if matches!(ctx.release_read_buf(n, buf, result), IoTaskStatus::Stop) {
            Poll::Ready(())
        } else {
            Poll::Pending
        };
    }
}

fn read_buf<T: AsyncRead>(
    io: Pin<&mut T>,
    cx: &mut Context<'_>,
    buf: &mut BytesVec,
) -> Poll<io::Result<usize>> {
    let n = {
        let dst =
            unsafe { &mut *(buf.chunk_mut() as *mut _ as *mut [mem::MaybeUninit<u8>]) };
        let mut buf = ReadBuf::uninit(dst);
        let ptr = buf.filled().as_ptr();
        if io.poll_read(cx, &mut buf)?.is_pending() {
            return Poll::Pending;
        }

        // Ensure the pointer does not change from under us
        assert_eq!(ptr, buf.filled().as_ptr());
        buf.filled().len()
    };

    // Safety: This is guaranteed to be the number of initialized (and read)
    // bytes due to the invariants provided by `ReadBuf::filled`.
    unsafe {
        buf.advance_mut(n);
    }

    Poll::Ready(Ok(n))
}

/// Flush write buffer to underlying I/O stream.
fn write_io<T: AsyncRead + AsyncWrite + Unpin>(
    io: &mut T,
    buf: &mut BytesVec,
    cx: &mut Context<'_>,
) -> Poll<io::Result<usize>> {
    let len = buf.len();

    if len != 0 {
        // log::trace!("Flushing framed transport: {len:?}");

        let mut written = 0;
        while let Poll::Ready(n) = Pin::new(&mut *io).poll_write(cx, &buf[written..])? {
            if n == 0 {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "failed to write frame to transport",
                )));
            } else {
                written += n;
                if written == len {
                    break;
                }
            }
        }
        // log::trace!("flushed {written} bytes");

        // flush
        if written > 0 {
            let _ = Pin::new(&mut *io).poll_flush(cx)?;
            Poll::Ready(Ok(written))
        } else {
            Poll::Pending
        }
    } else {
        Poll::Pending
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
        let len = self.0.with_read_buf(|src| {
            let len = cmp::min(src.len(), buf.remaining());
            buf.put_slice(&src.split_to(len));
            len
        });

        if len == 0 {
            match ready!(self.0.poll_read_ready(cx)) {
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
        Poll::Ready(self.0.write(buf).map(|_| buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.as_ref().0.poll_flush(cx, false)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.as_ref().0.poll_shutdown(cx)
    }
}

#[derive(Debug)]
/// Query TCP Io connections for a handle to set socket options
pub struct SocketOptions(Weak<RefCell<TcpStream>>);

impl SocketOptions {
    pub fn set_linger(&self, dur: Option<Millis>) -> io::Result<()> {
        self.try_self()
            .and_then(|s| s.borrow().set_linger(dur.map(|d| d.into())))
    }

    pub fn set_ttl(&self, ttl: u32) -> io::Result<()> {
        self.try_self().and_then(|s| s.borrow().set_ttl(ttl))
    }

    fn try_self(&self) -> io::Result<Rc<RefCell<TcpStream>>> {
        self.0
            .upgrade()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "socket is gone"))
    }
}
