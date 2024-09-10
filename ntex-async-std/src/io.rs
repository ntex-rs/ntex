use std::{any, cell::RefCell, future::poll_fn, io, pin::Pin, task::Context, task::Poll};

use async_std::io::{Read as ARead, Write as AWrite};
use ntex_bytes::{Buf, BufMut, BytesVec};
use ntex_io::{types, Handle, IoStream, ReadContext, WriteContext};
use ntex_util::{future::lazy, ready};

use crate::TcpStream;

impl IoStream for TcpStream {
    fn start(self, read: ReadContext, write: WriteContext) -> Option<Box<dyn Handle>> {
        let mut rio = Read(RefCell::new(self.clone()));
        async_std::task::spawn_local(async move {
            read.handle(&mut rio).await;
        });
        let mut wio = Write(RefCell::new(self.clone()));
        async_std::task::spawn_local(async move {
            write.handle(&mut wio).await;
        });
        Some(Box::new(self))
    }
}

impl Handle for TcpStream {
    fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>> {
        if id == any::TypeId::of::<types::PeerAddr>() {
            if let Ok(addr) = self.0.peer_addr() {
                return Some(Box::new(types::PeerAddr(addr)));
            }
        }
        None
    }
}

/// Read io task
struct Read(RefCell<TcpStream>);

impl ntex_io::AsyncRead for Read {
    async fn read(&mut self, mut buf: BytesVec) -> (BytesVec, io::Result<usize>) {
        // read data from socket
        let result = poll_fn(|cx| {
            let mut io = self.0.borrow_mut();
            poll_read_buf(Pin::new(&mut io.0), cx, &mut buf)
        })
        .await;
        (buf, result)
    }
}

struct Write(RefCell<TcpStream>);

impl ntex_io::AsyncWrite for Write {
    #[inline]
    async fn write(&mut self, mut buf: BytesVec) -> (BytesVec, io::Result<()>) {
        match lazy(|cx| flush_io(&mut self.0.borrow_mut().0, &mut buf, cx)).await {
            Poll::Ready(res) => (buf, res),
            Poll::Pending => (buf, Ok(())),
        }
    }

    #[inline]
    async fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }

    #[inline]
    async fn shutdown(&mut self) -> io::Result<()> {
        self.0.borrow().0.shutdown(std::net::Shutdown::Both)
    }
}

/// Flush write buffer to underlying I/O stream.
pub(super) fn flush_io<T: ARead + AWrite + Unpin>(
    io: &mut T,
    buf: &mut BytesVec,
    cx: &mut Context<'_>,
) -> Poll<io::Result<()>> {
    let len = buf.len();

    if len != 0 {
        // log::trace!("flushing framed transport: {:?}", buf.len());

        let mut written = 0;
        let result = loop {
            break match Pin::new(&mut *io).poll_write(cx, &buf[written..]) {
                Poll::Ready(Ok(n)) => {
                    if n == 0 {
                        log::trace!("Disconnected during flush, written {}", written);
                        Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "failed to write frame to transport",
                        )))
                    } else {
                        written += n;
                        if written == len {
                            buf.clear();
                            Poll::Ready(Ok(()))
                        } else {
                            continue;
                        }
                    }
                }
                Poll::Pending => {
                    // remove written data
                    buf.advance(written);
                    Poll::Pending
                }
                Poll::Ready(Err(e)) => {
                    log::trace!("Error during flush: {}", e);
                    Poll::Ready(Err(e))
                }
            };
        };
        // log::trace!("flushed {} bytes", written);

        // flush
        if written > 0 {
            match Pin::new(&mut *io).poll_flush(cx) {
                Poll::Ready(Ok(_)) => result,
                Poll::Pending => Poll::Pending,
                Poll::Ready(Err(e)) => {
                    log::trace!("error during flush: {}", e);
                    Poll::Ready(Err(e))
                }
            }
        } else {
            result
        }
    } else {
        Poll::Ready(Ok(()))
    }
}

pub fn poll_read_buf<T: ARead>(
    io: Pin<&mut T>,
    cx: &mut Context<'_>,
    buf: &mut BytesVec,
) -> Poll<io::Result<usize>> {
    let dst = unsafe { &mut *(buf.chunk_mut() as *mut _ as *mut [u8]) };
    let n = ready!(io.poll_read(cx, dst))?;

    // Safety: This is guaranteed to be the number of initialized (and read)
    // bytes due to the invariants provided by Read::poll_read() api
    unsafe {
        buf.advance_mut(n);
    }

    Poll::Ready(Ok(n))
}

#[cfg(unix)]
mod unixstream {
    use super::*;
    use crate::UnixStream;

    impl IoStream for UnixStream {
        fn start(self, read: ReadContext, write: WriteContext) -> Option<Box<dyn Handle>> {
            let mut rio = Read(RefCell::new(self.clone()));
            async_std::task::spawn_local(async move {
                read.handle(&mut rio).await;
            });
            let mut wio = Write(RefCell::new(self));
            async_std::task::spawn_local(async move {
                write.handle(&mut wio).await;
            });
            None
        }
    }

    /// Read io task
    struct Read(RefCell<UnixStream>);

    impl ntex_io::AsyncRead for Read {
        async fn read(&mut self, mut buf: BytesVec) -> (BytesVec, io::Result<usize>) {
            // read data from socket
            let result = poll_fn(|cx| {
                let mut io = self.0.borrow_mut();
                poll_read_buf(Pin::new(&mut io.0), cx, &mut buf)
            })
            .await;
            (buf, result)
        }
    }

    struct Write(RefCell<UnixStream>);

    impl ntex_io::AsyncWrite for Write {
        #[inline]
        async fn write(&mut self, mut buf: BytesVec) -> (BytesVec, io::Result<()>) {
            match lazy(|cx| flush_io(&mut self.0.borrow_mut().0, &mut buf, cx)).await {
                Poll::Ready(res) => (buf, res),
                Poll::Pending => (buf, Ok(())),
            }
        }

        #[inline]
        async fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }

        #[inline]
        async fn shutdown(&mut self) -> io::Result<()> {
            self.0.borrow().0.shutdown(std::net::Shutdown::Both)
        }
    }
}
