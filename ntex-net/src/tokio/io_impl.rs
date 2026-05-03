use std::task::{Context, Poll, ready};
use std::{any, cell::Cell, cmp, future::poll_fn, io, mem, pin::Pin, rc::Rc, rc::Weak};

use ntex_bytes::{BufMut, BytePage};
use ntex_io::{
    Buffer, Filter, Handle, Io, IoBoxed, IoContext, IoStream, IoTaskStatus, Readiness,
    types,
};
use ntex_util::time::Millis;
use tok_io::io::{AsyncRead, AsyncWrite, ReadBuf};
use tok_io::net::TcpStream;

impl IoStream for super::TcpStream {
    fn start(self, ctx: IoContext) -> Option<Box<dyn Handle>> {
        let io = Rc::new(Cell::new(Some(self.0)));
        tok_io::task::spawn_local(run_rd(io.clone(), ctx.clone()));
        tok_io::task::spawn_local(run_wrt(io.clone(), ctx));
        Some(Box::new(HandleWrapper(io)))
    }
}

#[cfg(unix)]
impl IoStream for super::UnixStream {
    fn start(self, ctx: IoContext) -> Option<Box<dyn Handle>> {
        let io = Rc::new(Cell::new(Some(self.0)));
        tok_io::task::spawn_local(run_rd(io.clone(), ctx.clone()));
        tok_io::task::spawn_local(run_wrt(io, ctx));
        None
    }
}

trait Stream: AsyncRead + AsyncWrite + Unpin {
    fn try_write(&self, buf: &[u8]) -> io::Result<usize>;

    fn try_write_vectored(&self, buf: &[io::IoSlice<'_>]) -> io::Result<usize>;
}

impl Stream for TcpStream {
    fn try_write(&self, buf: &[u8]) -> io::Result<usize> {
        TcpStream::try_write(self, buf)
    }

    fn try_write_vectored(&self, buf: &[io::IoSlice<'_>]) -> io::Result<usize> {
        TcpStream::try_write_vectored(self, buf)
    }
}

#[cfg(unix)]
impl Stream for tok_io::net::UnixStream {
    fn try_write(&self, buf: &[u8]) -> io::Result<usize> {
        tok_io::net::UnixStream::try_write(self, buf)
    }

    fn try_write_vectored(&self, buf: &[io::IoSlice<'_>]) -> io::Result<usize> {
        tok_io::net::UnixStream::try_write_vectored(self, buf)
    }
}

struct HandleWrapper(Rc<Cell<Option<TcpStream>>>);

impl Handle for HandleWrapper {
    fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>> {
        if id == any::TypeId::of::<types::PeerAddr>() {
            let inner = self.0.take().unwrap();
            let result = inner.peer_addr();
            self.0.set(Some(inner));
            if let Ok(addr) = result {
                return Some(Box::new(types::PeerAddr(addr)));
            }
        } else if id == any::TypeId::of::<SocketOptions>() {
            return Some(Box::new(SocketOptions(Rc::downgrade(&self.0))));
        }
        None
    }

    fn write(&self, ctx: &IoContext) {
        let mut inner = self.0.take().unwrap();
        let _ = write(&mut inner, ctx, None);
        self.0.set(Some(inner));
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Status {
    Shutdown,
    Terminate,
}

async fn run_rd<T>(io: Rc<Cell<Option<T>>>, ctx: IoContext)
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    let st = poll_fn(|cx| {
        let mut inner = io.take().unwrap();
        let result = match ctx.poll_read_ready(cx) {
            Poll::Ready(Readiness::Ready) => read(&mut inner, &ctx, cx),
            Poll::Ready(Readiness::Shutdown | Readiness::Terminate) => Poll::Ready(()),
            Poll::Pending => Poll::Pending,
        };
        io.set(Some(inner));
        result
    })
    .await;
}

async fn run_wrt<T>(io: Rc<Cell<Option<T>>>, ctx: IoContext)
where
    T: Stream,
{
    let st = poll_fn(|cx| {
        let mut inner = io.take().unwrap();
        let result = match ctx.poll_write_ready(cx) {
            Poll::Ready(Readiness::Ready) => write(&mut inner, &ctx, Some(cx)),
            Poll::Ready(Readiness::Shutdown) => Poll::Ready(Status::Shutdown),
            Poll::Ready(Readiness::Terminate) => Poll::Ready(Status::Terminate),
            Poll::Pending => Poll::Pending,
        };
        io.set(Some(inner));
        result
    })
    .await;

    log::trace!("{}: Shuting down io {:?}", ctx.tag(), ctx.is_stopped());
    if !ctx.is_stopped() {
        let flush = st == Status::Shutdown;
        poll_fn(|cx| {
            let mut inner = io.take().unwrap();
            let result =
                if write(&mut inner, &ctx, Some(cx)) == Poll::Ready(Status::Terminate) {
                    Poll::Ready(())
                } else {
                    ctx.shutdown(flush, cx)
                };
            io.set(Some(inner));
            result
        })
        .await;
    }

    let result = poll_fn(|cx| {
        let mut inner = io.take().unwrap();
        let result = Pin::new(&mut inner).poll_shutdown(cx);
        io.set(Some(inner));
        result
    })
    .await;

    log::trace!("{}: Shutdown complete, result {result:?}", ctx.tag());
    if !ctx.is_stopped() {
        ctx.stop(None);
    }
}

const MAX_WRITE_SIZE: usize = 64 * 1024;
const MAX_WRITE_ITEMS: usize = 16;

fn write<T>(io: &mut T, ctx: &IoContext, mut cx: Option<&mut Context<'_>>) -> Poll<Status>
where
    T: Stream,
{
    loop {
        let (cx2, result) = ctx.with_write_buf(|dst| {
            let mut pages: [Option<BytePage>; MAX_WRITE_ITEMS] = [
                None, None, None, None, None, None, None, None, None, None, None, None,
                None, None, None, None,
            ];
            let mut bufs: [mem::MaybeUninit<io::IoSlice<'_>>; MAX_WRITE_ITEMS] =
                [mem::MaybeUninit::uninit(); MAX_WRITE_ITEMS];

            let mut num = 0;
            let mut size = 0;
            while let Some(page) = dst.take() {
                size += page.len();

                // SAFETY: Page is stored in `pages` for lifetime of `bufs`
                bufs[num] = mem::MaybeUninit::new(io::IoSlice::new(unsafe {
                    mem::transmute::<&[u8], &[u8]>(page.as_ref())
                }));
                pages[num] = Some(page);

                num += 1;
                if num == MAX_WRITE_ITEMS || size >= MAX_WRITE_SIZE {
                    break;
                }
            }

            if num > 0 {
                // SAFETY: initialize in previous block
                let bufs =
                    unsafe { &*(&raw const bufs[..num] as *const [std::io::IoSlice<'_>]) };

                let (cx, result) = if let Some(cx) = cx {
                    let result = write_io(ctx, io, cx, bufs);
                    (Some(cx), result)
                } else {
                    (None, write_io2(ctx, io, bufs))
                };
                let mut written = if let Poll::Ready(Ok(n)) = result { n } else { 0 };

                // remove written bytes
                if written > 0 {
                    for page in pages[..num].iter_mut().flatten() {
                        let len = cmp::min(page.len(), written);
                        page.advance_to(len);
                        written -= len;
                        if written == 0 {
                            break;
                        }
                    }
                }
                // return unwritten data back to buffer
                for p in pages[..num].iter_mut().rev() {
                    if let Some(page) = p.take() {
                        dst.prepend(page);
                    }
                }

                (cx, Some(result.map(|res| res.map(|_| ()))))
            } else {
                (cx, None)
            }
        });

        cx = cx2;

        break if let Some(result) = result {
            match ctx.update_write_buf(result) {
                IoTaskStatus::Stop => Poll::Ready(Status::Terminate),
                IoTaskStatus::Pause => Poll::Pending,
                IoTaskStatus::Io => continue,
            }
        } else {
            Poll::Pending
        };
    }
}

/// Flush write buffer to underlying I/O stream.
fn write_io<T: Stream>(
    ctx: &IoContext,
    io: &mut T,
    cx: &mut Context<'_>,
    bufs: &[io::IoSlice<'_>],
) -> Poll<io::Result<usize>> {
    let n = if bufs.len() == 1 {
        ready!(Pin::new(&mut *io).poll_write(cx, &bufs[0]))?
    } else {
        ready!(Pin::new(&mut *io).poll_write_vectored(cx, bufs))?
    };
    if n == 0 {
        Poll::Ready(Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "failed to write frame to transport",
        )))
    } else {
        #[cfg(feature = "trace")]
        log::trace!("{}: Flushed {n} bytes from {} pages", ctx.tag(), bufs.len());

        let _ = Pin::new(&mut *io).poll_flush(cx)?;
        Poll::Ready(Ok(n))
    }
}

/// Flush write buffer to underlying I/O stream.
fn write_io2<T: Stream>(
    ctx: &IoContext,
    io: &mut T,
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
            log::trace!(
                "{}: Flushed early {n} bytes from {} pages",
                ctx.tag(),
                bufs.len()
            );
            Poll::Ready(Ok(n))
        }
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => Poll::Pending,
        Err(e) => Poll::Ready(Err(e)),
    }
}

fn read<T: AsyncRead + Unpin>(
    io: &mut T,
    ctx: &IoContext,
    cx: &mut Context<'_>,
) -> Poll<()> {
    let mut buf = ctx.get_read_buf();

    // read data from socket
    loop {
        ctx.resize_read_buf(&mut buf);
        let result = match read_buf(Pin::new(&mut *io), cx, &mut buf) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(0)) => Poll::Ready(Err(None)),
            Poll::Ready(Ok(_)) => continue,
            Poll::Ready(Err(err)) => Poll::Ready(Err(Some(err))),
        };

        return if matches!(ctx.release_read_buf(buf, result), IoTaskStatus::Stop) {
            Poll::Ready(())
        } else {
            Poll::Pending
        };
    }
}

fn read_buf<T: AsyncRead>(
    io: Pin<&mut T>,
    cx: &mut Context<'_>,
    buf: &mut Buffer,
) -> Poll<io::Result<usize>> {
    let n = {
        let dst = buf.chunk_mut().as_mut();
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

#[derive(Debug)]
/// Query TCP Io connections for a handle to set socket options
pub struct SocketOptions(Weak<Cell<Option<TcpStream>>>);

impl SocketOptions {
    #[deprecated = "`SO_LINGER` causes the socket to block the thread on drop"]
    pub fn set_linger(&self, dur: Option<Millis>) -> io::Result<()> {
        #[allow(deprecated)]
        {
            let inner = self.try_self()?;
            let io = inner.take().unwrap();
            io.set_linger(dur.map(Into::into))?;
            inner.set(Some(io));
            Ok(())
        }
    }

    pub fn set_ttl(&self, ttl: u32) -> io::Result<()> {
        let inner = self.try_self()?;
        let io = inner.take().unwrap();
        io.set_ttl(ttl)?;
        inner.set(Some(io));
        Ok(())
    }

    fn try_self(&self) -> io::Result<Rc<Cell<Option<TcpStream>>>> {
        self.0
            .upgrade()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "socket is gone"))
    }
}
