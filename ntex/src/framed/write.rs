use std::task::{Context, Poll};
use std::{cell::RefCell, future::Future, pin::Pin, rc::Rc};

use crate::codec::{AsyncRead, AsyncWrite, ReadBuf};
use crate::framed::State;
use crate::time::{sleep, Sleep};

#[derive(Debug)]
enum IoWriteState {
    Processing,
    Shutdown(Option<Sleep>, Shutdown),
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
        let disconnect_timeout = state.get_disconnect_timeout();
        let st = IoWriteState::Shutdown(disconnect_timeout.map(sleep), Shutdown::None);

        Self { st, io, state }
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
            self.state.wake_dispatcher();
            return Poll::Ready(());
        }

        match this.st {
            IoWriteState::Processing => {
                if this.state.is_io_shutdown() {
                    log::trace!("write task is instructed to shutdown");

                    let disconnect_timeout = this.state.get_disconnect_timeout();
                    this.st = IoWriteState::Shutdown(
                        disconnect_timeout.map(sleep),
                        Shutdown::None,
                    );
                    return self.poll(cx);
                }

                // flush framed instance
                match this.state.flush_io(&mut *this.io.borrow_mut(), cx) {
                    Poll::Pending | Poll::Ready(true) => Poll::Pending,
                    Poll::Ready(false) => Poll::Ready(()),
                }
            }
            IoWriteState::Shutdown(ref mut delay, ref mut st) => {
                // close WRITE side and wait for disconnect on read side.
                // use disconnect timeout, otherwise it could hang forever.
                loop {
                    match st {
                        Shutdown::None => {
                            // flush write buffer
                            let result =
                                this.state.flush_io(&mut *this.io.borrow_mut(), cx);
                            match result {
                                Poll::Ready(true) => {
                                    *st = Shutdown::Flushed;
                                    continue;
                                }
                                Poll::Ready(false) => {
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
                                let mut read_buf = ReadBuf::new(&mut buf);
                                match Pin::new(&mut *io).poll_read(cx, &mut read_buf) {
                                    Poll::Ready(Err(_)) | Poll::Ready(Ok(_))
                                        if read_buf.filled().is_empty() =>
                                    {
                                        this.state.set_wr_shutdown_complete();
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
                    this.state.set_wr_shutdown_complete();
                    log::trace!("write task is stopped after delay");
                    return Poll::Ready(());
                }
            }
        }
    }
}
