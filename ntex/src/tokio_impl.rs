use std::task::{Context, Poll};
use std::{cell::RefCell, future::Future, io, pin::Pin, rc::Rc};

use crate::codec::AsyncWrite;
use crate::rt::net::TcpStream as TokioTcpStream;
use crate::time::{sleep, Sleep};
use crate::util::{Buf, BufMut};

use super::{IoStream, ReadState, WriteReadiness, WriteState};

pub struct TcpStream(TokioTcpStream);

impl IoStream for TcpStream {
    fn start(self, read: ReadState, write: WriteState) {
        let io = Rc::new(RefCell::new(self.0));

        crate::rt::spawn(ReadTask::new(io.clone(), read));
        crate::rt::spawn(WriteTask::new(io, write));
    }
}

impl IoStream for TokioTcpStream {
    fn start(self, read: ReadState, write: WriteState) {
        let io = Rc::new(RefCell::new(self));

        crate::rt::spawn(ReadTask::new(io.clone(), read));
        crate::rt::spawn(WriteTask::new(io, write));
    }
}

/// Read io task
struct ReadTask {
    io: Rc<RefCell<TokioTcpStream>>,
    state: ReadState,
}

impl ReadTask {
    /// Create new read io task
    fn new(io: Rc<RefCell<TokioTcpStream>>, state: ReadState) -> Self {
        Self { io, state }
    }
}

impl Future for ReadTask {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.as_ref();

        match this.state.poll_ready(cx) {
            Poll::Ready(Err(())) => {
                log::trace!("read task is instructed to shutdown");
                Poll::Ready(())
            }
            Poll::Ready(Ok(())) => {
                let io = this.io.borrow();
                let pool = this.state.memory_pool();
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

                    let dst = unsafe { &mut *(buf.chunk_mut() as *mut _ as *mut [u8]) };
                    match poll_read_buf(&io, cx, dst) {
                        Poll::Pending => break,
                        Poll::Ready(Ok(n)) => {
                            if n == 0 {
                                log::trace!("io stream is disconnected");
                                this.state.release_read_buf(buf, new_bytes);
                                this.state.close(None);
                                return Poll::Ready(());
                            } else {
                                new_bytes += n;

                                unsafe {
                                    buf.advance_mut(n);
                                }
                                if buf.len() > hw {
                                    break;
                                }
                            }
                        }
                        Poll::Ready(Err(err)) => {
                            log::trace!("read task failed on io {:?}", err);
                            this.state.release_read_buf(buf, new_bytes);
                            this.state.close(Some(err));
                            return Poll::Ready(());
                        }
                    }
                }

                this.state.release_read_buf(buf, new_bytes);
                Poll::Pending
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

fn poll_read_buf(
    io: &TokioTcpStream,
    cx: &mut Context<'_>,
    buf: &mut [u8],
) -> Poll<io::Result<usize>> {
    if io.poll_read_ready(cx)?.is_ready() {
        match io.try_read(buf) {
            Ok(0) => Poll::Ready(Ok(0)),
            Ok(n) => Poll::Ready(Ok(n)),
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => Poll::Pending,
            Err(e) => Poll::Ready(Err(e)),
        }
    } else {
        Poll::Pending
    }
}

#[derive(Debug)]
enum IoWriteState {
    Processing,
    Shutdown(Option<Sleep>, Shutdown),
}

#[derive(Debug)]
enum Shutdown {
    None,
    Flushed,
    Stopping,
}

/// Write io task
struct WriteTask {
    st: IoWriteState,
    io: Rc<RefCell<TokioTcpStream>>,
    state: WriteState,
}

impl WriteTask {
    /// Create new write io task
    fn new(io: Rc<RefCell<TokioTcpStream>>, state: WriteState) -> Self {
        Self {
            io,
            state,
            st: IoWriteState::Processing,
        }
    }
}

impl Future for WriteTask {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.as_mut().get_mut();

        match this.st {
            IoWriteState::Processing => {
                match this.state.poll_ready(cx) {
                    Poll::Ready(Ok(())) => {
                        // flush framed instance
                        match flush_io(&this.io.borrow(), &this.state, cx) {
                            Poll::Pending | Poll::Ready(true) => Poll::Pending,
                            Poll::Ready(false) => Poll::Ready(()),
                        }
                    }
                    Poll::Ready(Err(WriteReadiness::Shutdown)) => {
                        log::trace!("write task is instructed to shutdown");

                        this.st = IoWriteState::Shutdown(
                            this.state.disconnect_timeout().map(sleep),
                            Shutdown::None,
                        );
                        self.poll(cx)
                    }
                    Poll::Ready(Err(WriteReadiness::Terminate)) => {
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
                            match flush_io(&this.io.borrow(), &this.state, cx) {
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
                            let io = this.io.borrow();
                            loop {
                                match poll_read_buf(&io, cx, &mut buf) {
                                    Poll::Ready(Err(e)) => {
                                        this.state.close(Some(e));
                                        log::trace!("write task is stopped");
                                        return Poll::Ready(());
                                    }
                                    Poll::Ready(Ok(n)) if n == 0 => {
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
                    if let Some(ref delay) = delay {
                        if delay.poll_elapsed(cx).is_pending() {
                            return Poll::Pending;
                        }
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
pub(super) fn flush_io(
    io: &TokioTcpStream,
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
        let mut written = 0;
        while written < len {
            match io.poll_write_ready(cx) {
                Poll::Ready(Ok(())) => match io.try_write(&buf[written..]) {
                    Ok(n) => {
                        if n == 0 {
                            log::trace!(
                                "Disconnected during flush, written {}",
                                written
                            );
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
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) => {
                        log::trace!("Error during flush: {}", e);
                        pool.release_write_buf(buf);
                        state.close(Some(e));
                        return Poll::Ready(false);
                    }
                },
                Poll::Ready(Err(e)) => {
                    log::trace!("Error during flush: {}", e);
                    pool.release_write_buf(buf);
                    state.close(Some(e));
                    return Poll::Ready(false);
                }
                Poll::Pending => break,
            }
        }

        // remove written data
        if written == len {
            buf.clear();
            state.release_write_buf(buf);
            Poll::Ready(true)
        } else {
            buf.advance(written);
            state.release_write_buf(buf);
            Poll::Pending
        }
    } else {
        Poll::Ready(true)
    }
}
