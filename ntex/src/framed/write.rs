use std::task::{Context, Poll};
use std::{cell::RefCell, future::Future, io, pin::Pin, rc::Rc, time::Duration};

use bytes::{Buf, BytesMut};

use crate::codec::{AsyncRead, AsyncWrite};
use crate::framed::State;
use crate::rt::time::{delay_for, Delay};

const HW: usize = 16 * 1024;

#[derive(Debug)]
enum IoWriteState {
    Processing,
    Shutdown(Option<Delay>, Shutdown),
}

#[derive(Debug)]
enum Shutdown {
    None,
    Flushed,
    Shutdown,
}

/// Write io task
pub struct WriteTask<T>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    st: IoWriteState,
    io: Rc<RefCell<T>>,
    state: State,
}

impl<T> WriteTask<T>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    /// Create new write io task
    pub fn new(io: Rc<RefCell<T>>, state: State) -> Self {
        Self {
            io,
            state,
            st: IoWriteState::Processing,
        }
    }

    /// Shutdown io stream
    pub fn shutdown(io: Rc<RefCell<T>>, state: State) -> Self {
        let disconnect_timeout = state.get_disconnect_timeout() as u64;
        let st = IoWriteState::Shutdown(
            if disconnect_timeout != 0 {
                Some(delay_for(Duration::from_millis(disconnect_timeout)))
            } else {
                None
            },
            Shutdown::None,
        );

        Self { io, st, state }
    }
}

impl<T> Future for WriteTask<T>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.as_mut().get_mut();

        // IO error occured
        if this.state.is_io_err() {
            log::trace!("write io is closed");
            return Poll::Ready(());
        } else if this.state.is_io_stop() {
            self.state.dsp_wake_task();
            return Poll::Ready(());
        }

        match this.st {
            IoWriteState::Processing => {
                if this.state.is_io_shutdown() {
                    log::trace!("write task is instructed to shutdown");

                    let disconnect_timeout = this.state.get_disconnect_timeout() as u64;
                    this.st = IoWriteState::Shutdown(
                        if disconnect_timeout != 0 {
                            Some(delay_for(Duration::from_millis(disconnect_timeout)))
                        } else {
                            None
                        },
                        Shutdown::None,
                    );
                    return self.poll(cx);
                }

                // flush framed instance
                let (result, len) = this.state.with_write_buf(|buf| {
                    (flush(&mut *this.io.borrow_mut(), buf, cx), buf.len())
                });

                match result {
                    Poll::Ready(Ok(_)) | Poll::Pending => {
                        if len < HW {
                            this.state.update_write_task()
                        }
                    }
                    Poll::Ready(Err(err)) => {
                        log::trace!("error during sending data: {:?}", err);
                        this.state.set_io_error(Some(err));
                        return Poll::Ready(());
                    }
                }
                this.state.register_write_task(cx.waker());
                Poll::Pending
            }
            IoWriteState::Shutdown(ref mut delay, ref mut st) => {
                // close io, closes WRITE side and wait for disconnect
                // on read side. we have to use disconnect timeout, otherwise it
                // could hang forever.
                loop {
                    match st {
                        Shutdown::None => {
                            // flush write buffer
                            let mut io = this.io.borrow_mut();
                            let result = this
                                .state
                                .with_write_buf(|buf| flush(&mut *io, buf, cx));

                            match result {
                                Poll::Ready(Ok(_)) => {
                                    *st = Shutdown::Flushed;
                                    continue;
                                }
                                Poll::Ready(Err(_)) => {
                                    this.state.set_wr_shutdown_complete();
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
                                    *st = Shutdown::Shutdown;
                                    continue;
                                }
                                Poll::Ready(Err(_)) => {
                                    this.state.set_wr_shutdown_complete();
                                    log::trace!(
                                        "write task is closed with err during shutdown"
                                    );
                                    return Poll::Ready(());
                                }
                                _ => (),
                            }
                        }
                        Shutdown::Shutdown => {
                            // read until 0 or err
                            let mut buf = [0u8; 512];
                            let mut io = this.io.borrow_mut();
                            loop {
                                match Pin::new(&mut *io).poll_read(cx, &mut buf) {
                                    Poll::Ready(Ok(0)) | Poll::Ready(Err(_)) => {
                                        this.state.set_wr_shutdown_complete();
                                        log::trace!("write task is closed");
                                        return Poll::Ready(());
                                    }
                                    Poll::Pending => break,
                                    _ => (),
                                }
                            }
                        }
                    }

                    if let Some(ref mut delay) = delay {
                        futures::ready!(Pin::new(delay).poll(cx));
                    }
                    this.state.set_wr_shutdown_complete();
                    log::trace!("write task is closed after delay");
                    return Poll::Ready(());
                }
            }
        }
    }
}

/// Flush write buffer to underlying I/O stream.
pub(super) fn flush<T>(
    io: &mut T,
    buf: &mut BytesMut,
    cx: &mut Context<'_>,
) -> Poll<io::Result<()>>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    let len = buf.len();

    if len != 0 {
        // log::trace!("flushing framed transport: {}", len);

        let mut written = 0;
        while written < len {
            match Pin::new(&mut *io).poll_write(cx, &buf[written..]) {
                Poll::Pending => break,
                Poll::Ready(Ok(n)) => {
                    if n == 0 {
                        log::trace!("Disconnected during flush, written {}", written);
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "failed to write frame to transport",
                        )));
                    } else {
                        written += n
                    }
                }
                Poll::Ready(Err(e)) => {
                    log::trace!("Error during flush: {}", e);
                    return Poll::Ready(Err(e));
                }
            }
        }
        // log::trace!("flushed {} bytes", written);

        // remove written data
        if written == len {
            // SAFETY: flushed same amount as in buffer, we dont need to reallocate
            unsafe { buf.set_len(0) }
        } else {
            buf.advance(written);
        }
    }

    // flush
    futures::ready!(Pin::new(&mut *io).poll_flush(cx))?;

    if buf.is_empty() {
        Poll::Ready(Ok(()))
    } else {
        Poll::Pending
    }
}
