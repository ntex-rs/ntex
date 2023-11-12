use std::task::{Context, Poll};
use std::{any, cell::RefCell, cmp, future::Future, io, mem, pin::Pin, rc::Rc, rc::Weak};

use ntex_bytes::{Buf, BufMut, BytesVec};
use ntex_io::{
    types, Filter, Handle, Io, IoBoxed, IoStream, ReadContext, ReadStatus, WriteContext,
    WriteStatus,
};
use ntex_util::{ready, time::sleep, time::Millis, time::Sleep};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;

impl IoStream for crate::TcpStream {
    fn start(self, read: ReadContext, write: WriteContext) -> Option<Box<dyn Handle>> {
        let io = Rc::new(RefCell::new(self.0));

        tokio::task::spawn_local(ReadTask::new(io.clone(), read));
        tokio::task::spawn_local(WriteTask::new(io.clone(), write));
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
struct ReadTask {
    io: Rc<RefCell<TcpStream>>,
    state: ReadContext,
}

impl ReadTask {
    /// Create new read io task
    fn new(io: Rc<RefCell<TcpStream>>, state: ReadContext) -> Self {
        Self { io, state }
    }
}

impl Future for ReadTask {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.as_ref();

        match ready!(this.state.poll_ready(cx)) {
            ReadStatus::Ready => {
                this.state.with_buf(|buf, hw, lw| {
                    // read data from socket
                    let mut io = this.io.borrow_mut();
                    loop {
                        // make sure we've got room
                        let remaining = buf.remaining_mut();
                        if remaining < lw {
                            buf.reserve(hw - remaining);
                        }
                        return match poll_read_buf(Pin::new(&mut *io), cx, buf) {
                            Poll::Pending => Poll::Pending,
                            Poll::Ready(Ok(n)) => {
                                if n == 0 {
                                    log::trace!("tcp stream is disconnected");
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
                })
            }
            ReadStatus::Terminate => {
                log::trace!("read task is instructed to shutdown");
                Poll::Ready(())
            }
        }
    }
}

#[derive(Debug)]
enum IoWriteState {
    Processing(Option<Sleep>),
    Shutdown(Sleep, Shutdown),
}

#[derive(Debug)]
enum Shutdown {
    None,
    Flushed,
    Stopping(u16),
}

/// Write io task
struct WriteTask {
    st: IoWriteState,
    io: Rc<RefCell<TcpStream>>,
    state: WriteContext,
}

impl WriteTask {
    /// Create new write io task
    fn new(io: Rc<RefCell<TcpStream>>, state: WriteContext) -> Self {
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
                            &mut *this.io.borrow_mut(),
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

                        this.st = IoWriteState::Shutdown(timeout, Shutdown::None);
                        self.poll(cx)
                    }
                    Poll::Ready(WriteStatus::Terminate) => {
                        log::trace!("write task is instructed to terminate");

                        if !matches!(
                            this.io.borrow().linger(),
                            Ok(Some(std::time::Duration::ZERO))
                        ) {
                            // call shutdown to prevent flushing data on terminated Io. when
                            // linger is set to zero, closing will reset the connection, so
                            // shutdown is not neccessary.
                            let _ = Pin::new(&mut *this.io.borrow_mut()).poll_shutdown(cx);
                        }
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
                        Shutdown::None => {
                            // flush write buffer
                            let mut io = this.io.borrow_mut();
                            match this.state.with_buf(|buf| flush_io(&mut *io, buf, cx)) {
                                Poll::Ready(Ok(())) => {
                                    *st = Shutdown::Flushed;
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
                        Shutdown::Flushed => {
                            // shutdown WRITE side
                            match Pin::new(&mut *this.io.borrow_mut()).poll_shutdown(cx) {
                                Poll::Ready(Ok(_)) => {
                                    *st = Shutdown::Stopping(0);
                                    continue;
                                }
                                Poll::Ready(Err(e)) => {
                                    log::trace!(
                                        "write task is closed with err during shutdown"
                                    );
                                    this.state.close(Some(e));
                                    return Poll::Ready(());
                                }
                                _ => (),
                            }
                        }
                        Shutdown::Stopping(ref mut count) => {
                            // read until 0 or err
                            let mut buf = [0u8; 512];
                            loop {
                                let mut read_buf = ReadBuf::new(&mut buf);
                                match Pin::new(&mut *this.io.borrow_mut())
                                    .poll_read(cx, &mut read_buf)
                                {
                                    Poll::Ready(Err(_)) | Poll::Ready(Ok(_))
                                        if read_buf.filled().is_empty() =>
                                    {
                                        this.state.close(None);
                                        log::trace!("tokio write task is stopped");
                                        return Poll::Ready(());
                                    }
                                    Poll::Pending => {
                                        *count += read_buf.filled().len() as u16;
                                        if *count > 4096 {
                                            log::trace!("tokio write task is stopped, too much input");
                                            this.state.close(None);
                                            return Poll::Ready(());
                                        }
                                        break;
                                    }
                                    _ => (),
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
            // log::trace!("flushed {} bytes", written);

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

            tokio::task::spawn_local(ReadTask::new(io.clone(), read));
            tokio::task::spawn_local(WriteTask::new(io, write));
            None
        }
    }

    /// Read io task
    struct ReadTask {
        io: Rc<RefCell<UnixStream>>,
        state: ReadContext,
    }

    impl ReadTask {
        /// Create new read io task
        fn new(io: Rc<RefCell<UnixStream>>, state: ReadContext) -> Self {
            Self { io, state }
        }
    }

    impl Future for ReadTask {
        type Output = ();

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let this = self.as_ref();

            this.state.with_buf(|buf, hw, lw| {
                match ready!(this.state.poll_ready(cx)) {
                    ReadStatus::Ready => {
                        // read data from socket
                        let mut io = this.io.borrow_mut();
                        loop {
                            // make sure we've got room
                            let remaining = buf.remaining_mut();
                            if remaining < lw {
                                buf.reserve(hw - remaining);
                            }

                            return match poll_read_buf(Pin::new(&mut *io), cx, buf) {
                                Poll::Pending => Poll::Pending,
                                Poll::Ready(Ok(n)) => {
                                    if n == 0 {
                                        log::trace!("tokio unix stream is disconnected");
                                        Poll::Ready(Ok(()))
                                    } else if buf.len() < hw {
                                        continue;
                                    } else {
                                        Poll::Pending
                                    }
                                }
                                Poll::Ready(Err(err)) => {
                                    log::trace!("unix stream read task failed {:?}", err);
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
    struct WriteTask {
        st: IoWriteState,
        io: Rc<RefCell<UnixStream>>,
        state: WriteContext,
    }

    impl WriteTask {
        /// Create new write io task
        fn new(io: Rc<RefCell<UnixStream>>, state: WriteContext) -> Self {
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
                                &mut *this.io.borrow_mut(),
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

                            this.st = IoWriteState::Shutdown(timeout, Shutdown::None);
                            self.poll(cx)
                        }
                        Poll::Ready(WriteStatus::Terminate) => {
                            log::trace!("write task is instructed to terminate");

                            let _ = Pin::new(&mut *this.io.borrow_mut()).poll_shutdown(cx);
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
                            Shutdown::None => {
                                // flush write buffer
                                let mut io = this.io.borrow_mut();
                                match this.state.with_buf(|buf| flush_io(&mut *io, buf, cx))
                                {
                                    Poll::Ready(Ok(())) => {
                                        *st = Shutdown::Flushed;
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
                            Shutdown::Flushed => {
                                // shutdown WRITE side
                                match Pin::new(&mut *this.io.borrow_mut()).poll_shutdown(cx)
                                {
                                    Poll::Ready(Ok(_)) => {
                                        *st = Shutdown::Stopping(0);
                                        continue;
                                    }
                                    Poll::Ready(Err(e)) => {
                                        log::trace!(
                                            "write task is closed with err during shutdown"
                                        );
                                        this.state.close(Some(e));
                                        return Poll::Ready(());
                                    }
                                    _ => (),
                                }
                            }
                            Shutdown::Stopping(ref mut count) => {
                                // read until 0 or err
                                let mut buf = [0u8; 512];
                                loop {
                                    let mut read_buf = ReadBuf::new(&mut buf);
                                    match Pin::new(&mut *this.io.borrow_mut())
                                        .poll_read(cx, &mut read_buf)
                                    {
                                        Poll::Ready(Err(_)) | Poll::Ready(Ok(_))
                                            if read_buf.filled().is_empty() =>
                                        {
                                            this.state.close(None);
                                            log::trace!("write task is stopped");
                                            return Poll::Ready(());
                                        }
                                        Poll::Pending => {
                                            *count += read_buf.filled().len() as u16;
                                            if *count > 4096 {
                                                log::trace!(
                                                    "write task is stopped, too much input"
                                                );
                                                this.state.close(None);
                                                return Poll::Ready(());
                                            }
                                            break;
                                        }
                                        _ => (),
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
                        return Poll::Ready(());
                    }
                }
            }
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
