use std::task::{Context, Poll};
use std::{any, cell::RefCell, cmp, future::poll_fn, io, mem, pin::Pin, rc::Rc, rc::Weak};

use ntex_bytes::{Buf, BufMut, BytesVec};
use ntex_io::{
    types, Filter, Handle, Io, IoBoxed, IoStream, ReadContext, WriteContext,
    WriteContextBuf,
};
use ntex_util::{ready, time::Millis};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;

impl IoStream for crate::TcpStream {
    fn start(self, read: ReadContext, write: WriteContext) -> Option<Box<dyn Handle>> {
        let io = Rc::new(RefCell::new(self.0));

        let mut rio = Read(io.clone());
        tokio::task::spawn_local(async move {
            read.handle(&mut rio).await;
        });
        let mut wio = Write(io.clone());
        tokio::task::spawn_local(async move {
            write.handle(&mut wio).await;
        });
        Some(Box::new(HandleWrapper(io)))
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

/// Read io task
struct Read(Rc<RefCell<TcpStream>>);

impl ntex_io::AsyncRead for Read {
    #[inline]
    async fn read(&mut self, mut buf: BytesVec) -> (BytesVec, io::Result<usize>) {
        // read data from socket
        let result = poll_fn(|cx| {
            let mut io = self.0.borrow_mut();
            poll_read_buf(Pin::new(&mut *io), cx, &mut buf)
        })
        .await;
        (buf, result)
    }
}

struct Write(Rc<RefCell<TcpStream>>);

impl ntex_io::AsyncWrite for Write {
    #[inline]
    async fn write(&mut self, buf: &mut WriteContextBuf) -> io::Result<()> {
        poll_fn(|cx| {
            if let Some(mut b) = buf.take() {
                let result = flush_io(&mut *self.0.borrow_mut(), &mut b, cx);
                buf.set(b);
                result
            } else {
                Poll::Ready(Ok(()))
            }
        })
        .await
    }

    #[inline]
    async fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }

    #[inline]
    async fn shutdown(&mut self) -> io::Result<()> {
        poll_fn(|cx| Pin::new(&mut *self.0.borrow_mut()).poll_shutdown(cx)).await
    }
}

/// Flush write buffer to underlying I/O stream.
pub(super) fn flush_io<T: AsyncRead + AsyncWrite + Unpin>(
    io: &mut T,
    buf: &mut BytesVec,
    cx: &mut Context<'_>,
) -> Poll<io::Result<()>> {
    let len = buf.len();

    if len != 0 {
        // log::trace!("{}: Flushing framed transport: {:?}", st.tag(), buf.len());

        let mut written = 0;
        let result = loop {
            break match Pin::new(&mut *io).poll_write(cx, &buf[written..]) {
                Poll::Ready(Ok(n)) => {
                    if n == 0 {
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
                Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            };
        };
        // log::trace!("{}: flushed {} bytes", st.tag(), written);

        // flush
        if written > 0 {
            match Pin::new(&mut *io).poll_flush(cx) {
                Poll::Ready(Ok(_)) => result,
                Poll::Pending => Poll::Pending,
                Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            }
        } else {
            result
        }
    } else {
        Poll::Ready(Ok(()))
    }
}

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

#[cfg(unix)]
mod unixstream {
    use tokio::net::UnixStream;

    use super::*;

    impl IoStream for crate::UnixStream {
        fn start(self, read: ReadContext, write: WriteContext) -> Option<Box<dyn Handle>> {
            let io = Rc::new(RefCell::new(self.0));

            let mut rio = Read(io.clone());
            tokio::task::spawn_local(async move {
                read.handle(&mut rio).await;
            });
            let mut wio = Write(io.clone());
            tokio::task::spawn_local(async move {
                write.handle(&mut wio).await;
            });
            None
        }
    }

    struct Read(Rc<RefCell<UnixStream>>);

    impl ntex_io::AsyncRead for Read {
        #[inline]
        async fn read(&mut self, mut buf: BytesVec) -> (BytesVec, io::Result<usize>) {
            // read data from socket
            let result = poll_fn(|cx| {
                let mut io = self.0.borrow_mut();
                poll_read_buf(Pin::new(&mut *io), cx, &mut buf)
            })
            .await;
            (buf, result)
        }
    }

    struct Write(Rc<RefCell<UnixStream>>);

    impl ntex_io::AsyncWrite for Write {
        #[inline]
        async fn write(&mut self, buf: &mut WriteContextBuf) -> io::Result<()> {
            poll_fn(|cx| {
                if let Some(mut b) = buf.take() {
                    let result = flush_io(&mut *self.0.borrow_mut(), &mut b, cx);
                    buf.set(b);
                    result
                } else {
                    Poll::Ready(Ok(()))
                }
            })
            .await
        }

        #[inline]
        async fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }

        #[inline]
        async fn shutdown(&mut self) -> io::Result<()> {
            poll_fn(|cx| Pin::new(&mut *self.0.borrow_mut()).poll_shutdown(cx)).await
        }
    }
}

pub fn poll_read_buf<T: AsyncRead>(
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
