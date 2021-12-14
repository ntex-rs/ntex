use std::task::{Context, Poll};
use std::{cell::RefCell, future::Future, io, pin::Pin, rc::Rc};

use ntex_bytes::{Buf, BufMut};
use ntex_util::time::{sleep, Sleep};
use tok_io::{io::AsyncRead, io::AsyncWrite, io::ReadBuf};

use super::{IoStream, ReadState, WriteReadiness, WriteState};

impl<T> IoStream for T
where
    T: AsyncRead + AsyncWrite + Unpin + 'static,
{
    fn start(self, read: ReadState, write: WriteState) {
        let io = Rc::new(RefCell::new(self));

        ntex_util::spawn(ReadTask::new(io.clone(), read));
        ntex_util::spawn(WriteTask::new(io, write));
    }
}

/// Read io task
struct ReadTask<T> {
    io: Rc<RefCell<T>>,
    state: ReadState,
}

impl<T> ReadTask<T>
where
    T: AsyncRead + AsyncWrite + Unpin + 'static,
{
    /// Create new read io task
    fn new(io: Rc<RefCell<T>>, state: ReadState) -> Self {
        Self { io, state }
    }
}

impl<T> Future for ReadTask<T>
where
    T: AsyncRead + AsyncWrite + Unpin + 'static,
{
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.as_ref();

        match this.state.poll_ready(cx) {
            Poll::Ready(Err(())) => {
                log::trace!("read task is instructed to shutdown");
                Poll::Ready(())
            }
            Poll::Ready(Ok(())) => {
                let pool = this.state.memory_pool();
                let mut io = this.io.borrow_mut();
                let mut buf = self.state.get_read_buf();
                let (hw, lw) = pool.read_params().unpack();

                // read data from socket
                let mut new_bytes = 0;
                loop {
                    // make sure we've got room
                    let remaining = buf.remaining_mut();
                    if remaining < lw {
                        buf.reserve(hw - remaining);
                    }

                    match ntex_codec::poll_read_buf(Pin::new(&mut *io), cx, &mut buf) {
                        Poll::Pending => break,
                        Poll::Ready(Ok(n)) => {
                            if n == 0 {
                                log::trace!("io stream is disconnected");
                                if let Err(e) =
                                    this.state.release_read_buf(buf, new_bytes)
                                {
                                    this.state.close(Some(e));
                                } else {
                                    this.state.close(None);
                                }
                                return Poll::Ready(());
                            } else {
                                new_bytes += n;
                                if buf.len() > hw {
                                    break;
                                }
                            }
                        }
                        Poll::Ready(Err(err)) => {
                            log::trace!("read task failed on io {:?}", err);
                            let _ = this.state.release_read_buf(buf, new_bytes);
                            this.state.close(Some(err));
                            return Poll::Ready(());
                        }
                    }
                }

                if let Err(e) = this.state.release_read_buf(buf, new_bytes) {
                    this.state.close(Some(e));
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            }
            Poll::Pending => Poll::Pending,
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
    Stopping,
}

/// Write io task
struct WriteTask<T> {
    st: IoWriteState,
    io: Rc<RefCell<T>>,
    state: WriteState,
}

impl<T> WriteTask<T>
where
    T: AsyncRead + AsyncWrite + Unpin + 'static,
{
    /// Create new write io task
    fn new(io: Rc<RefCell<T>>, state: WriteState) -> Self {
        Self {
            io,
            state,
            st: IoWriteState::Processing(None),
        }
    }
}

impl<T> Future for WriteTask<T>
where
    T: AsyncRead + AsyncWrite + Unpin + 'static,
{
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.as_mut().get_mut();

        match this.st {
            IoWriteState::Processing(ref mut delay) => {
                match this.state.poll_ready(cx) {
                    Poll::Ready(Ok(())) => {
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
                    Poll::Ready(Err(WriteReadiness::Timeout(time))) => {
                        if delay.is_none() {
                            *delay = Some(sleep(time));
                        }
                        self.poll(cx)
                    }
                    Poll::Ready(Err(WriteReadiness::Shutdown(time))) => {
                        log::trace!("write task is instructed to shutdown");

                        let timeout = if let Some(delay) = delay.take() {
                            delay
                        } else {
                            sleep(time)
                        };

                        this.st = IoWriteState::Shutdown(timeout, Shutdown::None);
                        self.poll(cx)
                    }
                    Poll::Ready(Err(WriteReadiness::Terminate)) => {
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
                            match Pin::new(&mut *this.io.borrow_mut()).poll_shutdown(cx)
                            {
                                Poll::Ready(Ok(_)) => {
                                    *st = Shutdown::Stopping;
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
                        Shutdown::Stopping => {
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
                                    Poll::Pending => break,
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
    state: &WriteState,
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
        // log::trace!("flushing framed transport: {:?}", buf);

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
        // log::trace!("flushed {} bytes", written);

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
