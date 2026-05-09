use std::task::{Context, Poll, ready};
use std::{any, cmp, future::poll_fn, io, mem, pin::Pin, ptr, rc::Rc};

use ntex_bytes::{BufMut, BytePage};
use ntex_io::{
    Filter, Handle, Io, IoBoxed, IoContext, IoStream, IoTaskStatus, Readiness, types,
};
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
        let _ = write(self.0.as_ref(), ctx);
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
        let _ = write(self.0.as_ref(), ctx);
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Status {
    Shutdown,
    Terminate,
}

async fn run_rd<T>(io: Rc<T>, ctx: IoContext)
where
    T: Stream + Unpin,
{
    let st = poll_fn(|cx| {
        loop {
            return match ready!(ctx.poll_read_ready(cx)) {
                Readiness::Ready => match ready!(io.as_ref().poll_read_ready(cx)) {
                    Ok(()) => match read(io.as_ref(), &ctx) {
                        IoTaskStatus::Pause => Poll::Pending,
                        IoTaskStatus::Io => continue,
                        IoTaskStatus::Stop => Poll::Ready(()),
                    },
                    Err(err) => {
                        ctx.stop(Some(err));
                        Poll::Ready(())
                    }
                },
                Readiness::Shutdown | Readiness::Terminate => Poll::Ready(()),
            };
        }
    })
    .await;
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum WrtStatus {
    More,
    Pending,
    Terminate,
}

async fn run_wrt<T>(io: Rc<T>, ctx: IoContext)
where
    T: Stream,
{
    let st = poll_fn(|cx| {
        loop {
            return match ready!(ctx.poll_write_ready(cx)) {
                Readiness::Ready => match ready!(io.poll_write_ready(cx)) {
                    Ok(()) => match write(io.as_ref(), &ctx) {
                        WrtStatus::Pending => Poll::Pending,
                        WrtStatus::More => continue,
                        WrtStatus::Terminate => Poll::Ready(Status::Terminate),
                    },
                    Err(err) => {
                        ctx.update_write_status(Poll::Ready(Err(err)));
                        Poll::Ready(Status::Terminate)
                    }
                },
                Readiness::Shutdown => Poll::Ready(Status::Shutdown),
                Readiness::Terminate => Poll::Ready(Status::Terminate),
            };
        }
    })
    .await;

    log::trace!("{}: Shuting down io {:?}", ctx.tag(), ctx.is_stopped());
    if !ctx.is_stopped() {
        let flush = st == Status::Shutdown;
        poll_fn(|cx| match ready!(io.poll_write_ready(cx)) {
            Ok(()) => {
                if write(io.as_ref(), &ctx) == WrtStatus::Terminate {
                    Poll::Ready(())
                } else {
                    ctx.shutdown(flush, cx)
                }
            }
            Err(err) => {
                ctx.update_write_status(Poll::Ready(Err(err)));
                Poll::Ready(())
            }
        })
        .await;
    }

    log::trace!("{}: Shutdown complete", ctx.tag());
    if !ctx.is_stopped() {
        ctx.stop(None);
    }
}

const MAX_WRITE_SIZE: usize = 64 * 1024;
const MAX_WRITE_ITEMS: usize = 16;

fn write<T>(io: &T, ctx: &IoContext) -> WrtStatus
where
    T: Stream,
{
    loop {
        let (more, result) = ctx.with_write_buf(|dst| {
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

                let result = write_io(ctx, io, bufs);
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

                (!dst.is_empty(), result.map(|res| res.map(|_| ())))
            } else {
                (false, Poll::Ready(Ok(())))
            }
        });

        let pending = result.is_pending();
        break match ctx.update_write_status(result) {
            IoTaskStatus::Stop => WrtStatus::Terminate,
            IoTaskStatus::Pause => WrtStatus::Pending,
            IoTaskStatus::Io => {
                if pending && more {
                    WrtStatus::More
                } else {
                    continue;
                }
            }
        };
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

fn read<T: Stream + Unpin>(io: &T, ctx: &IoContext) -> IoTaskStatus {
    let mut buf = ctx.get_read_buf();

    // read data from socket
    loop {
        ctx.resize_read_buf(&mut buf);

        let io_res =
            io.try_read(unsafe { &mut *(ptr::from_mut(buf.chunk_mut()) as *mut [u8]) });

        let result = match io_res {
            Ok(0) => Poll::Ready(Err(None)),
            Ok(n) => {
                // Safety: This is guaranteed to be the number of initialized
                // bytes due to the invariants provided by `try_read()`.
                unsafe {
                    buf.advance_mut(n);
                }
                continue;
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Poll::Pending,
            Err(e) => Poll::Ready(Err(Some(e))),
        };

        return ctx.release_read_buf(buf, result);
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
