use std::{any, io};

use compio::buf::{BufResult, IoBuf, IoBufMut, SetBufInit};
use compio::io::{AsyncRead, AsyncWrite};
use compio::net::TcpStream;
use ntex_bytes::{Buf, BufMut, BytesVec};
use ntex_io::{types, Handle, IoStream, ReadContext, WriteContext, WriteContextBuf};

impl IoStream for crate::TcpStream {
    fn start(self, read: ReadContext, write: WriteContext) -> Option<Box<dyn Handle>> {
        let io = self.0.clone();
        compio::runtime::spawn(async move { run(io.clone(), &read, write).await }).detach();

        Some(Box::new(HandleWrapper(self.0)))
    }
}

#[cfg(unix)]
impl IoStream for crate::UnixStream {
    fn start(self, read: ReadContext, write: WriteContext) -> Option<Box<dyn Handle>> {
        compio::runtime::spawn(async move { run(self.0.clone(), &read, write).await })
            .detach();

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
        self.0.set_len(len + self.0.len());
    }
}

async fn run<T: AsyncRead + AsyncWrite + Clone + 'static>(
    io: T,
    read: &ReadContext,
    write: WriteContext,
) {
    let mut wr_io = WriteIo(io.clone());
    let wr_task = compio::runtime::spawn(async move {
        write.handle(&mut wr_io).await;
        log::debug!("{} Write task is stopped", write.tag());
    });
    let mut io = ReadIo(io);

    read.handle(&mut io).await;
    log::debug!("{} Read task is stopped", read.tag());

    if !wr_task.is_finished() {
        let _ = wr_task.await;
    }
}

struct ReadIo<T>(T);

impl<T> ntex_io::AsyncRead for ReadIo<T>
where
    T: AsyncRead,
{
    #[inline]
    async fn read(&mut self, buf: BytesVec) -> (BytesVec, io::Result<usize>) {
        let BufResult(result, buf) = self.0.read(CompioBuf(buf)).await;
        (buf.0, result)
    }
}

struct WriteIo<T>(T);

impl<T> ntex_io::AsyncWrite for WriteIo<T>
where
    T: AsyncWrite,
{
    #[inline]
    async fn write(&mut self, wbuf: &mut WriteContextBuf) -> io::Result<()> {
        if let Some(b) = wbuf.take() {
            let mut buf = CompioBuf(b);
            loop {
                let BufResult(result, buf1) = self.0.write(buf).await;
                buf = buf1;

                let result = match result {
                    Ok(0) => Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "failed to write frame to transport",
                    )),
                    Ok(size) => {
                        buf.0.advance(size);
                        if buf.0.is_empty() {
                            Ok(())
                        } else {
                            continue;
                        }
                    }
                    Err(e) => Err(e),
                };
                wbuf.set(buf.0);
                return result;
            }
        } else {
            Ok(())
        }
    }

    #[inline]
    async fn flush(&mut self) -> io::Result<()> {
        self.0.flush().await
    }

    #[inline]
    async fn shutdown(&mut self) -> io::Result<()> {
        self.0.shutdown().await
    }
}
