use std::{any, cmp, future::poll_fn, io, mem, task::Poll};

use compio_buf::{BufResult, IoBuf, IoBufMut, SetLen};
use compio_io::{AsyncRead, AsyncWrite};
use ntex_bytes::{BufMut, BytePage, BytePages};
use ntex_io::{Buffer, Handle, IoContext, IoStream, IoTaskStatus, Readiness, types};
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

struct CompioBuf(Buffer);

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
    ctx.resize_read_buf(&mut buf);
    let mut read_fut = Some(Box::pin(read_buf(&io, buf)));

    loop {
        if read_ready(ctx).await {
            break;
        }

        match select(read_fut.as_mut().unwrap(), not_read_ready(ctx)).await {
            Either::Left(BufResult(result, cbuf)) => {
                let result = ctx.release_read_buf(
                    cbuf.0,
                    Poll::Ready(result.map(|_| ()).map_err(Some)),
                );
                if result == IoTaskStatus::Stop {
                    break;
                }
                let mut buf = ctx.get_read_buf();
                ctx.resize_read_buf(&mut buf);
                read_fut = Some(Box::pin(read_buf(&io, buf)));
            }
            Either::Right(true) => break,
            Either::Right(false) => (),
        }
    }

    log::trace!("{}: Shuting down io {:?}", ctx.tag(), ctx.is_stopped());
    if !ctx.is_stopped() {
        let result = poll_fn(|cx| ctx.shutdown(true, cx)).await;
        log::trace!("{}: Shuting down complete {result:?}", ctx.tag());
        ctx.stop(None);
    }
}

async fn read_buf<T>(io: &T, buf: Buffer) -> BufResult<usize, CompioBuf>
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
    T: AsyncRead + AsyncWrite + Clone,
{
    loop {
        match poll_fn(|cx| ctx.poll_write_ready(cx)).await {
            Readiness::Ready => {
                let bufs = ctx.with_write_buf(build_bufs);
                if write_buf(&mut io, ctx, bufs).await == IoTaskStatus::Stop {
                    let _ = io.shutdown().await;
                    break;
                }
            }
            Readiness::Shutdown => {
                let bufs = ctx.with_write_buf(build_bufs);
                write_buf(&mut io, ctx, bufs).await;
                let _ = io.shutdown().await;
                break;
            }
            Readiness::Terminate => return,
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

async fn write_buf<T>(
    io: &mut T,
    ctx: &IoContext,
    mut bufs: Vec<CompioPage>,
) -> IoTaskStatus
where
    T: AsyncRead + AsyncWrite,
{
    while !bufs.is_empty() {
        let result = if bufs.len() == 1 {
            let BufResult(result, buf) = io.write(bufs.pop().unwrap()).await;
            bufs.push(buf);
            result
        } else {
            let BufResult(result, bufs1) = io.write_vectored(bufs).await;
            bufs = bufs1;
            result
        };

        let result = match result {
            Ok(0) => Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "failed to write frame to transport",
            )),
            Ok(mut written) => {
                // remove written pages
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
                Ok(())
            }
            Err(e) => Err(e),
        };
        if ctx.update_write_status(Poll::Ready(result)) == IoTaskStatus::Stop {
            return IoTaskStatus::Stop;
        }
    }
    IoTaskStatus::Io
}
