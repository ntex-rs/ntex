use std::task::{Context, Poll};
use std::{any, cell::RefCell, cmp, future::Future, io, mem, pin::Pin, rc::Rc};

use ntex_bytes::{Buf, BufMut, BytesMut};
use ntex_util::{ready, time::sleep, time::Sleep};
use tok_io::io::{AsyncRead, AsyncWrite, ReadBuf};
use tok_io::net::TcpStream;

use crate::{
    types, Filter, Handle, Io, IoBoxed, IoStream, ReadContext, ReadStatus, WriteContext,
    WriteStatus,
};

impl IoStream for TcpStream {
    fn start(self, read: ReadContext, write: WriteContext) -> Option<Box<dyn Handle>> {
        let io = Rc::new(RefCell::new(self));

        tok_io::task::spawn_local(ReadTask::new(io.clone(), read));
        tok_io::task::spawn_local(WriteTask::new(io.clone(), write));
        Some(Box::new(io))
    }
}

impl Handle for Rc<RefCell<TcpStream>> {
    fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>> {
        if id == any::TypeId::of::<types::PeerAddr>() {
            if let Ok(addr) = self.borrow().peer_addr() {
                return Some(Box::new(types::PeerAddr(addr)));
            }
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

        loop {
            match ready!(this.state.poll_ready(cx)) {
                ReadStatus::Ready => {
                    let pool = this.state.memory_pool();
                    let mut io = this.io.borrow_mut();
                    let mut buf = self.state.get_read_buf();
                    let (hw, lw) = pool.read_params().unpack();

                    // read data from socket
                    let mut new_bytes = 0;
                    let mut close = false;
                    let mut pending = false;
                    loop {
                        // make sure we've got room
                        let remaining = buf.remaining_mut();
                        if remaining < lw {
                            buf.reserve(hw - remaining);
                        }

                        match poll_read_buf(Pin::new(&mut *io), cx, &mut buf) {
                            Poll::Pending => {
                                pending = true;
                                break;
                            }
                            Poll::Ready(Ok(n)) => {
                                if n == 0 {
                                    log::trace!("tokio stream is disconnected");
                                    close = true;
                                } else {
                                    new_bytes += n;
                                    if new_bytes <= hw {
                                        continue;
                                    }
                                }
                                break;
                            }
                            Poll::Ready(Err(err)) => {
                                log::trace!("read task failed on io {:?}", err);
                                let _ = this.state.release_read_buf(buf, new_bytes);
                                this.state.close(Some(err));
                                return Poll::Ready(());
                            }
                        }
                    }

                    if new_bytes == 0 && close {
                        this.state.close(None);
                        return Poll::Ready(());
                    }
                    this.state.release_read_buf(buf, new_bytes);
                    return if close {
                        this.state.close(None);
                        Poll::Ready(())
                    } else if pending {
                        Poll::Pending
                    } else {
                        continue;
                    };
                }
                ReadStatus::Terminate => {
                    log::trace!("read task is instructed to shutdown");
                    return Poll::Ready(());
                }
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
        let mut this = self.as_mut().get_mut();

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

                        // flush framed instance
                        match flush_io(&mut *this.io.borrow_mut(), &this.state, cx) {
                            Poll::Pending | Poll::Ready(true) => Poll::Pending,
                            Poll::Ready(false) => Poll::Ready(()),
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
                            match flush_io(&mut *this.io.borrow_mut(), &this.state, cx) {
                                Poll::Ready(true) => {
                                    *st = Shutdown::Flushed;
                                    continue;
                                }
                                Poll::Ready(false) => {
                                    log::trace!(
                                        "write task is closed with err during flush"
                                    );
                                    return Poll::Ready(());
                                }
                                _ => (),
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
                            let mut io = this.io.borrow_mut();
                            loop {
                                let mut read_buf = ReadBuf::new(&mut buf);
                                match Pin::new(&mut *io).poll_read(cx, &mut read_buf) {
                                    Poll::Ready(Err(_)) | Poll::Ready(Ok(_))
                                        if read_buf.filled().is_empty() =>
                                    {
                                        this.state.close(None);
                                        log::trace!("write task is stopped");
                                        return Poll::Ready(());
                                    }
                                    Poll::Pending => {
                                        *count += read_buf.filled().len() as u16;
                                        if *count > 8196 {
                                            log::trace!(
                                                "write task is stopped, too much input"
                                            );
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
    state: &WriteContext,
    cx: &mut Context<'_>,
) -> Poll<bool> {
    let mut buf = if let Some(buf) = state.get_write_buf() {
        buf
    } else {
        return Poll::Ready(true);
    };
    let len = buf.len();
    let pool = state.memory_pool();

    if len != 0 {
        // log::trace!("flushing framed transport: {:?}", buf.len());

        let mut written = 0;
        while written < len {
            match Pin::new(&mut *io).poll_write(cx, &buf[written..]) {
                Poll::Pending => break,
                Poll::Ready(Ok(n)) => {
                    if n == 0 {
                        log::trace!("Disconnected during flush, written {}", written);
                        pool.release_write_buf(buf);
                        state.close(Some(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "failed to write frame to transport",
                        )));
                        return Poll::Ready(false);
                    } else {
                        written += n
                    }
                }
                Poll::Ready(Err(e)) => {
                    log::trace!("Error during flush: {}", e);
                    pool.release_write_buf(buf);
                    state.close(Some(e));
                    return Poll::Ready(false);
                }
            }
        }
        log::trace!("flushed {} bytes", written);

        // remove written data
        let result = if written == len {
            buf.clear();
            if let Err(e) = state.release_write_buf(buf) {
                state.close(Some(e));
                return Poll::Ready(false);
            }
            Poll::Ready(true)
        } else {
            buf.advance(written);
            if let Err(e) = state.release_write_buf(buf) {
                state.close(Some(e));
                return Poll::Ready(false);
            }
            Poll::Pending
        };

        // flush
        match Pin::new(&mut *io).poll_flush(cx) {
            Poll::Ready(Ok(_)) => result,
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => {
                log::trace!("error during flush: {}", e);
                state.close(Some(e));
                Poll::Ready(false)
            }
        }
    } else {
        Poll::Ready(true)
    }
}

impl<F: Filter> AsyncRead for Io<F> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let len = self.with_read_buf(|src| {
            let len = cmp::min(src.len(), buf.remaining());
            buf.put_slice(&src.split_to(len));
            len
        });

        if len == 0 {
            match ready!(self.poll_read_ready(cx)) {
                Ok(Some(())) => Poll::Pending,
                Ok(None) => Poll::Ready(Ok(())),
                Err(e) => Poll::Ready(Err(e)),
            }
        } else {
            Poll::Ready(Ok(()))
        }
    }
}

impl<F: Filter> AsyncWrite for Io<F> {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(self.write(buf).map(|_| buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Io::poll_flush(&*self, cx, false)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Io::poll_shutdown(&*self, cx)
    }
}

impl AsyncRead for IoBoxed {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let len = self.with_read_buf(|src| {
            let len = cmp::min(src.len(), buf.remaining());
            buf.put_slice(&src.split_to(len));
            len
        });

        if len == 0 {
            match ready!(self.poll_read_ready(cx)) {
                Ok(Some(())) => Poll::Pending,
                Err(e) => Poll::Ready(Err(e)),
                Ok(None) => Poll::Ready(Ok(())),
            }
        } else {
            Poll::Ready(Ok(()))
        }
    }
}

impl AsyncWrite for IoBoxed {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(self.write(buf).map(|_| buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        (&*self.as_ref()).poll_flush(cx, false)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        (&*self.as_ref()).poll_shutdown(cx)
    }
}

#[cfg(unix)]
mod unixstream {
    use tok_io::net::UnixStream;

    use super::*;

    impl IoStream for UnixStream {
        fn start(self, read: ReadContext, write: WriteContext) -> Option<Box<dyn Handle>> {
            let io = Rc::new(RefCell::new(self));

            tok_io::task::spawn_local(ReadTask::new(io.clone(), read));
            tok_io::task::spawn_local(WriteTask::new(io, write));
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

            loop {
                match ready!(this.state.poll_ready(cx)) {
                    ReadStatus::Ready => {
                        let pool = this.state.memory_pool();
                        let mut io = this.io.borrow_mut();
                        let mut buf = self.state.get_read_buf();
                        let (hw, lw) = pool.read_params().unpack();

                        // read data from socket
                        let mut new_bytes = 0;
                        let mut close = false;
                        let mut pending = false;
                        loop {
                            // make sure we've got room
                            let remaining = buf.remaining_mut();
                            if remaining < lw {
                                buf.reserve(hw - remaining);
                            }

                            match poll_read_buf(Pin::new(&mut *io), cx, &mut buf) {
                                Poll::Pending => {
                                    pending = true;
                                    break;
                                }
                                Poll::Ready(Ok(n)) => {
                                    if n == 0 {
                                        log::trace!("unix stream is disconnected");
                                        close = true;
                                    } else {
                                        new_bytes += n;
                                        if new_bytes <= hw {
                                            continue;
                                        }
                                    }
                                    break;
                                }
                                Poll::Ready(Err(err)) => {
                                    log::trace!("read task failed on io {:?}", err);
                                    let _ = this.state.release_read_buf(buf, new_bytes);
                                    this.state.close(Some(err));
                                    return Poll::Ready(());
                                }
                            }
                        }

                        if new_bytes == 0 && close {
                            this.state.close(None);
                            return Poll::Ready(());
                        }
                        this.state.release_read_buf(buf, new_bytes);
                        return if close {
                            this.state.close(None);
                            Poll::Ready(())
                        } else if pending {
                            Poll::Pending
                        } else {
                            continue;
                        };
                    }
                    ReadStatus::Terminate => {
                        log::trace!("read task is instructed to shutdown");
                        return Poll::Ready(());
                    }
                }
            }
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
            let mut this = self.as_mut().get_mut();

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

                            // flush framed instance
                            match flush_io(&mut *this.io.borrow_mut(), &this.state, cx) {
                                Poll::Pending | Poll::Ready(true) => Poll::Pending,
                                Poll::Ready(false) => Poll::Ready(()),
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
                                match flush_io(&mut *this.io.borrow_mut(), &this.state, cx)
                                {
                                    Poll::Ready(true) => {
                                        *st = Shutdown::Flushed;
                                        continue;
                                    }
                                    Poll::Ready(false) => {
                                        log::trace!(
                                            "write task is closed with err during flush"
                                        );
                                        return Poll::Ready(());
                                    }
                                    _ => (),
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
                                let mut io = this.io.borrow_mut();
                                loop {
                                    let mut read_buf = ReadBuf::new(&mut buf);
                                    match Pin::new(&mut *io).poll_read(cx, &mut read_buf) {
                                        Poll::Ready(Err(_)) | Poll::Ready(Ok(_))
                                            if read_buf.filled().is_empty() =>
                                        {
                                            this.state.close(None);
                                            log::trace!("write task is stopped");
                                            return Poll::Ready(());
                                        }
                                        Poll::Pending => {
                                            *count += read_buf.filled().len() as u16;
                                            if *count > 8196 {
                                                log::trace!(
                                                    "write task is stopped, too much input"
                                                );
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
    buf: &mut BytesMut,
) -> Poll<io::Result<usize>> {
    if !buf.has_remaining_mut() {
        return Poll::Ready(Ok(0));
    }

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
