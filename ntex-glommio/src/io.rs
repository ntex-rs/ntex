use std::task::{Context, Poll};
use std::{any, future::Future, io, pin::Pin};

use futures_lite::future::FutureExt;
use futures_lite::io::{AsyncRead, AsyncWrite};
use ntex_bytes::{Buf, BufMut, BytesVec};
use ntex_io::{
    types, Handle, IoStream, ReadContext, ReadStatus, WriteContext, WriteStatus,
};
use ntex_util::{ready, time::sleep, time::Sleep};

use crate::net_impl::{TcpStream, UnixStream};

impl IoStream for TcpStream {
    fn start(self, read: ReadContext, write: WriteContext) -> Option<Box<dyn Handle>> {
        glommio::spawn_local(ReadTask::new(self.clone(), read)).detach();
        glommio::spawn_local(WriteTask::new(self.clone(), write)).detach();
        Some(Box::new(self))
    }
}

impl IoStream for UnixStream {
    fn start(self, read: ReadContext, write: WriteContext) -> Option<Box<dyn Handle>> {
        glommio::spawn_local(UnixReadTask::new(self.clone(), read)).detach();
        glommio::spawn_local(UnixWriteTask::new(self, write)).detach();
        None
    }
}

impl Handle for TcpStream {
    fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>> {
        if id == any::TypeId::of::<types::PeerAddr>() {
            if let Ok(addr) = self.0.borrow().peer_addr() {
                return Some(Box::new(types::PeerAddr(addr)));
            }
        }
        None
    }
}

/// Read io task
struct ReadTask {
    io: TcpStream,
    state: ReadContext,
}

impl ReadTask {
    /// Create new read io task
    fn new(io: TcpStream, state: ReadContext) -> Self {
        Self { io, state }
    }
}

impl Future for ReadTask {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.as_mut();

        this.state.with_buf(|buf, hw, lw| {
            match ready!(this.state.poll_ready(cx)) {
                ReadStatus::Ready => {
                    // read data from socket
                    loop {
                        // make sure we've got room
                        let remaining = buf.remaining_mut();
                        if remaining < lw {
                            buf.reserve(hw - remaining);
                        }

                        return match poll_read_buf(
                            Pin::new(&mut *this.io.0.borrow_mut()),
                            cx,
                            buf,
                        ) {
                            Poll::Pending => Poll::Pending,
                            Poll::Ready(Ok(n)) => {
                                if n == 0 {
                                    log::trace!("glommio stream is disconnected");
                                    Poll::Ready(Ok(()))
                                } else if buf.len() < hw {
                                    continue;
                                } else {
                                    Poll::Pending
                                }
                            }
                            Poll::Ready(Err(err)) => {
                                log::trace!("read task failed on io {:?}", err);
                                Poll::Ready(Err(err))
                            }
                        };
                    }
                }
                ReadStatus::Terminate => {
                    log::trace!("read task is instructed to shutdown");
                    Poll::Ready(Ok(()))
                }
            }
        })
    }
}

enum IoWriteState {
    Processing(Option<Sleep>),
    Shutdown(Sleep, Shutdown),
}

enum Shutdown {
    Flush,
    Close(Pin<Box<dyn Future<Output = glommio::Result<(), ()>>>>),
    Stopping(u16),
}

/// Write io task
struct WriteTask {
    st: IoWriteState,
    io: TcpStream,
    state: WriteContext,
}

impl WriteTask {
    /// Create new write io task
    fn new(io: TcpStream, state: WriteContext) -> Self {
        Self {
            io,
            state,
            st: IoWriteState::Processing(None),
        }
    }
}

impl Future for WriteTask {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.as_mut().get_mut();

        match this.st {
            IoWriteState::Processing(ref mut delay) => {
                match this.state.poll_ready(cx) {
                    Poll::Ready(WriteStatus::Ready) => {
                        if let Some(delay) = delay {
                            if delay.poll_elapsed(cx).is_ready() {
                                this.state.close(Some(io::Error::new(
                                    io::ErrorKind::TimedOut,
                                    "Operation timedout",
                                )));
                                return Poll::Ready(());
                            }
                        }

                        // flush io stream
                        match ready!(this.state.with_buf(|buf| flush_io(
                            &mut *this.io.0.borrow_mut(),
                            buf,
                            cx
                        ))) {
                            Ok(()) => Poll::Pending,
                            Err(e) => {
                                this.state.close(Some(e));
                                Poll::Ready(())
                            }
                        }
                    }
                    Poll::Ready(WriteStatus::Timeout(time)) => {
                        log::trace!("initiate timeout delay for {:?}", time);
                        if delay.is_none() {
                            *delay = Some(sleep(time));
                        }
                        self.poll(cx)
                    }
                    Poll::Ready(WriteStatus::Shutdown(time)) => {
                        log::trace!("write task is instructed to shutdown");

                        let timeout = if let Some(delay) = delay.take() {
                            delay
                        } else {
                            sleep(time)
                        };

                        this.st = IoWriteState::Shutdown(timeout, Shutdown::Flush);
                        self.poll(cx)
                    }
                    Poll::Ready(WriteStatus::Terminate) => {
                        log::trace!("write task is instructed to terminate");

                        let _ = Pin::new(&mut *this.io.0.borrow_mut()).poll_close(cx);
                        this.state.close(None);
                        Poll::Ready(())
                    }
                    Poll::Pending => Poll::Pending,
                }
            }
            IoWriteState::Shutdown(ref mut delay, ref mut st) => {
                // close WRITE side and wait for disconnect on read side.
                // use disconnect timeout, otherwise it could hang forever.
                loop {
                    match st {
                        Shutdown::Flush => {
                            // flush write buffer
                            let mut io = this.io.0.borrow_mut();
                            match this.state.with_buf(|buf| flush_io(&mut *io, buf, cx)) {
                                Poll::Ready(Ok(())) => {
                                    let io = this.io.clone();
                                    let fut = Box::pin(async move {
                                        let fut =
                                            io.0.borrow()
                                                .shutdown(std::net::Shutdown::Write);
                                        fut.await
                                    });
                                    *st = Shutdown::Close(fut);
                                    continue;
                                }
                                Poll::Ready(Err(err)) => {
                                    log::trace!(
                                        "write task is closed with err during flush, {:?}",
                                        err
                                    );
                                    this.state.close(Some(err));
                                    return Poll::Ready(());
                                }
                                Poll::Pending => (),
                            }
                        }
                        Shutdown::Close(ref mut fut) => {
                            if ready!(fut.poll(cx)).is_err() {
                                this.state.close(None);
                                return Poll::Ready(());
                            }
                            *st = Shutdown::Stopping(0);
                            continue;
                        }
                        Shutdown::Stopping(ref mut count) => {
                            // read until 0 or err
                            let mut buf = [0u8; 512];
                            let io = &mut this.io;
                            loop {
                                match Pin::new(&mut *io.0.borrow_mut())
                                    .poll_read(cx, &mut buf)
                                {
                                    Poll::Ready(Err(e)) => {
                                        log::trace!("write task is stopped");
                                        this.state.close(Some(e));
                                        return Poll::Ready(());
                                    }
                                    Poll::Ready(Ok(0)) => {
                                        log::trace!("glommio socket is disconnected");
                                        this.state.close(None);
                                        return Poll::Ready(());
                                    }
                                    Poll::Ready(Ok(n)) => {
                                        *count += n as u16;
                                        if *count > 4096 {
                                            log::trace!(
                                                "write task is stopped, too much input"
                                            );
                                            this.state.close(None);
                                            return Poll::Ready(());
                                        }
                                    }
                                    Poll::Pending => break,
                                }
                            }
                        }
                    }

                    // disconnect timeout
                    if delay.poll_elapsed(cx).is_pending() {
                        return Poll::Pending;
                    }
                    log::trace!("write task is stopped after delay");
                    this.state.close(None);
                    let _ = Pin::new(&mut *this.io.0.borrow_mut()).poll_close(cx);
                    return Poll::Ready(());
                }
            }
        }
    }
}

/// Flush write buffer to underlying I/O stream.
pub(super) fn flush_io<T: AsyncRead + AsyncWrite + Unpin>(
    io: &mut T,
    buf: &mut Option<BytesVec>,
    cx: &mut Context<'_>,
) -> Poll<io::Result<()>> {
    if let Some(buf) = buf {
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
            log::trace!("flushed {} bytes", written);

            // flush
            return if written > 0 {
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
            };
        }
    }
    Poll::Ready(Ok(()))
}

pub fn poll_read_buf<T: AsyncRead>(
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

/// Read io task
struct UnixReadTask {
    io: UnixStream,
    state: ReadContext,
}

impl UnixReadTask {
    /// Create new read io task
    fn new(io: UnixStream, state: ReadContext) -> Self {
        Self { io, state }
    }
}

impl Future for UnixReadTask {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.as_mut();

        this.state.with_buf(|buf, hw, lw| {
            match ready!(this.state.poll_ready(cx)) {
                ReadStatus::Ready => {
                    // read data from socket
                    loop {
                        // make sure we've got room
                        let remaining = buf.remaining_mut();
                        if remaining < lw {
                            buf.reserve(hw - remaining);
                        }

                        return match poll_read_buf(
                            Pin::new(&mut *this.io.0.borrow_mut()),
                            cx,
                            buf,
                        ) {
                            Poll::Pending => Poll::Pending,
                            Poll::Ready(Ok(n)) => {
                                if n == 0 {
                                    log::trace!("glommio stream is disconnected");
                                    Poll::Ready(Ok(()))
                                } else if buf.len() < hw {
                                    continue;
                                } else {
                                    Poll::Pending
                                }
                            }
                            Poll::Ready(Err(err)) => {
                                log::trace!("read task failed on io {:?}", err);
                                Poll::Ready(Err(err))
                            }
                        };
                    }
                }
                ReadStatus::Terminate => {
                    log::trace!("read task is instructed to shutdown");
                    Poll::Ready(Ok(()))
                }
            }
        })
    }
}

/// Write io task
struct UnixWriteTask {
    st: IoWriteState,
    io: UnixStream,
    state: WriteContext,
}

impl UnixWriteTask {
    /// Create new write io task
    fn new(io: UnixStream, state: WriteContext) -> Self {
        Self {
            io,
            state,
            st: IoWriteState::Processing(None),
        }
    }
}

impl Future for UnixWriteTask {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.as_mut().get_mut();

        match this.st {
            IoWriteState::Processing(ref mut delay) => {
                match this.state.poll_ready(cx) {
                    Poll::Ready(WriteStatus::Ready) => {
                        if let Some(delay) = delay {
                            if delay.poll_elapsed(cx).is_ready() {
                                this.state.close(Some(io::Error::new(
                                    io::ErrorKind::TimedOut,
                                    "Operation timedout",
                                )));
                                return Poll::Ready(());
                            }
                        }

                        // flush io stream
                        match ready!(this.state.with_buf(|buf| flush_io(
                            &mut *this.io.0.borrow_mut(),
                            buf,
                            cx
                        ))) {
                            Ok(()) => Poll::Pending,
                            Err(e) => {
                                this.state.close(Some(e));
                                Poll::Ready(())
                            }
                        }
                    }
                    Poll::Ready(WriteStatus::Timeout(time)) => {
                        log::trace!("initiate timeout delay for {:?}", time);
                        if delay.is_none() {
                            *delay = Some(sleep(time));
                        }
                        self.poll(cx)
                    }
                    Poll::Ready(WriteStatus::Shutdown(time)) => {
                        log::trace!("write task is instructed to shutdown");

                        let timeout = if let Some(delay) = delay.take() {
                            delay
                        } else {
                            sleep(time)
                        };

                        this.st = IoWriteState::Shutdown(timeout, Shutdown::Flush);
                        self.poll(cx)
                    }
                    Poll::Ready(WriteStatus::Terminate) => {
                        log::trace!("write task is instructed to terminate");

                        let _ = Pin::new(&mut *this.io.0.borrow_mut()).poll_close(cx);
                        this.state.close(None);
                        Poll::Ready(())
                    }
                    Poll::Pending => Poll::Pending,
                }
            }
            IoWriteState::Shutdown(ref mut delay, ref mut st) => {
                // close WRITE side and wait for disconnect on read side.
                // use disconnect timeout, otherwise it could hang forever.
                loop {
                    match st {
                        Shutdown::Flush => {
                            // flush write buffer
                            let mut io = this.io.0.borrow_mut();
                            match this.state.with_buf(|buf| flush_io(&mut *io, buf, cx)) {
                                Poll::Ready(Ok(())) => {
                                    let io = this.io.clone();
                                    let fut = Box::pin(async move {
                                        let fut =
                                            io.0.borrow()
                                                .shutdown(std::net::Shutdown::Write);
                                        fut.await
                                    });
                                    *st = Shutdown::Close(fut);
                                    continue;
                                }
                                Poll::Ready(Err(err)) => {
                                    log::trace!(
                                        "write task is closed with err during flush, {:?}",
                                        err
                                    );
                                    this.state.close(Some(err));
                                    return Poll::Ready(());
                                }
                                Poll::Pending => (),
                            }
                        }
                        Shutdown::Close(ref mut fut) => {
                            if ready!(fut.poll(cx)).is_err() {
                                this.state.close(None);
                                return Poll::Ready(());
                            }
                            *st = Shutdown::Stopping(0);
                            continue;
                        }
                        Shutdown::Stopping(ref mut count) => {
                            // read until 0 or err
                            let mut buf = [0u8; 512];
                            let io = &mut this.io;
                            loop {
                                match Pin::new(&mut *io.0.borrow_mut())
                                    .poll_read(cx, &mut buf)
                                {
                                    Poll::Ready(Err(e)) => {
                                        log::trace!("write task is stopped");
                                        this.state.close(Some(e));
                                        return Poll::Ready(());
                                    }
                                    Poll::Ready(Ok(0)) => {
                                        log::trace!("glommio unix socket is disconnected");
                                        this.state.close(None);
                                        return Poll::Ready(());
                                    }
                                    Poll::Ready(Ok(n)) => {
                                        *count += n as u16;
                                        if *count > 4096 {
                                            log::trace!(
                                                "write task is stopped, too much input"
                                            );
                                            this.state.close(None);
                                            return Poll::Ready(());
                                        }
                                    }
                                    Poll::Pending => break,
                                }
                            }
                        }
                    }

                    // disconnect timeout
                    if delay.poll_elapsed(cx).is_pending() {
                        return Poll::Pending;
                    }
                    log::trace!("write task is stopped after delay");
                    this.state.close(None);
                    let _ = Pin::new(&mut *this.io.0.borrow_mut()).poll_close(cx);
                    return Poll::Ready(());
                }
            }
        }
    }
}
