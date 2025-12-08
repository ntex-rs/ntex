use std::{any, future::poll_fn, io, task::Poll};

use compio_buf::{BufResult, IoBuf, IoBufMut, SetBufInit};
use compio_io::{AsyncRead, AsyncWrite};
use compio_net::TcpStream;
use ntex_bytes::{Buf, BufMut, BytesVec};
use ntex_io::{Handle, IoContext, IoStream, IoTaskStatus, Readiness, types};
use ntex_util::future::{Either, select};

impl IoStream for crate::compat::TcpStream {
    fn start(self, ctx: IoContext) -> Option<Box<dyn Handle>> {
        compio_runtime::spawn(run(self.0.clone(), ctx)).detach();
        Some(Box::new(HandleWrapper(self.0)))
    }
}

impl IoStream for crate::compat::UnixStream {
    fn start(self, ctx: IoContext) -> Option<Box<dyn Handle>> {
        compio_runtime::spawn(run(self.0.clone(), ctx)).detach();
        None
    }
}

struct HandleWrapper(TcpStream);

impl Handle for HandleWrapper {
    fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>> {
        if id == any::TypeId::of::<types::PeerAddr>() {
            if let Ok(addr) = self.0.peer_addr() {
                return Some(Box::new(types::PeerAddr(addr)));
            }
        }
        None
    }
}

struct CompioBuf(BytesVec);

unsafe impl IoBuf for CompioBuf {
    #[inline]
    fn as_buf_ptr(&self) -> *const u8 {
        self.0.chunk().as_ptr()
    }

    #[inline]
    fn buf_len(&self) -> usize {
        self.0.len()
    }

    #[inline]
    fn buf_capacity(&self) -> usize {
        self.0.remaining_mut()
    }
}

unsafe impl IoBufMut for CompioBuf {
    fn as_buf_mut_ptr(&mut self) -> *mut u8 {
        self.0.chunk_mut().as_mut_ptr()
    }
}

impl SetBufInit for CompioBuf {
    unsafe fn set_buf_init(&mut self, len: usize) {
        unsafe {
            self.0.set_len(len + self.0.len());
        }
    }
}

async fn run<T: AsyncRead + AsyncWrite + Clone + Unpin + 'static>(io: T, ctx: IoContext) {
    let wr_io = io.clone();
    let wr_ctx = ctx.clone();
    let wr_task = compio_runtime::spawn(async move {
        write(wr_io, &wr_ctx).await;
        log::debug!("{} Write task is stopped", wr_ctx.tag());
    });

    read(io, &ctx).await;
    log::debug!("{} Read task is stopped", ctx.tag());

    if !wr_task.is_finished() {
        let _ = wr_task.await;
    }
}

async fn read<T>(io: T, ctx: &IoContext)
where
    T: AsyncRead + AsyncWrite + Clone + Unpin,
{
    let mut buf = ctx.get_read_buf();
    let mut total = buf.len();
    ctx.resize_read_buf(&mut buf);
    let mut read_fut = Some(read_buf(&io, buf));

    loop {
        if read_ready(ctx).await {
            break;
        }

        match select(read_fut.take().unwrap(), not_read_ready(ctx)).await {
            Either::Left(BufResult(result, cbuf)) => {
                let nbytes = cbuf.0.len() - total;
                let result = ctx.release_read_buf(
                    nbytes,
                    cbuf.0,
                    Poll::Ready(result.map(|_| ()).map_err(Some)),
                );
                if result == IoTaskStatus::Stop {
                    break;
                } else {
                    let mut buf = ctx.get_read_buf();
                    total = buf.len();
                    ctx.resize_read_buf(&mut buf);
                    read_fut = Some(read_buf(&io, buf));
                };
            }
            Either::Right(()) => break,
        }
    }
}

async fn read_buf<T>(io: &T, buf: BytesVec) -> BufResult<usize, CompioBuf>
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

async fn not_read_ready(ctx: &IoContext) {
    poll_fn(|cx| match ctx.poll_read_ready(cx) {
        Poll::Pending | Poll::Ready(Readiness::Ready) => Poll::Pending,
        Poll::Ready(_) => Poll::Ready(()),
    })
    .await
}

async fn write<T>(mut io: T, ctx: &IoContext)
where
    T: AsyncRead + AsyncWrite + Clone,
{
    loop {
        match poll_fn(|cx| ctx.poll_write_ready(cx)).await {
            Readiness::Ready => {
                if write_buf(&mut io, ctx, ctx.get_write_buf()).await == IoTaskStatus::Stop
                {
                    let _ = io.shutdown().await;
                    break;
                }
            }
            Readiness::Shutdown => {
                write_buf(&mut io, ctx, ctx.get_write_buf()).await;
                let _ = io.shutdown().await;
                break;
            }
            Readiness::Terminate => return,
        }
    }
}

async fn write_buf<T>(io: &mut T, ctx: &IoContext, buf: Option<BytesVec>) -> IoTaskStatus
where
    T: AsyncRead + AsyncWrite,
{
    if let Some(b) = buf {
        let len = b.len();
        let mut buf = CompioBuf(b);

        loop {
            let BufResult(result, buf1) = io.write(buf).await;
            buf = buf1;

            let result = match result {
                Ok(0) => Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "failed to write frame to transport",
                )),
                Ok(size) => {
                    buf.0.advance(size);
                    if buf.0.is_empty() {
                        Ok(len)
                    } else {
                        continue;
                    }
                }
                Err(e) => Err(e),
            };
            buf.0.clear();
            unsafe { buf.0.set_len(len) };
            return ctx.release_write_buf(buf.0, Poll::Ready(result));
        }
    } else {
        IoTaskStatus::Io
    }
}
