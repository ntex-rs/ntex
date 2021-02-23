use std::{
    cell::RefCell, future::Future, io, pin::Pin, rc::Rc, task::Context, task::Poll,
};

use bytes::BytesMut;

use crate::codec::{AsyncRead, AsyncWrite};
use crate::framed::State;

const LW: usize = 1024;
const HW: usize = 8 * 1024;

/// Read io task
pub struct ReadTask<T>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    io: Rc<RefCell<T>>,
    state: State,
}

impl<T> ReadTask<T>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    /// Create new read io task
    pub fn new(io: Rc<RefCell<T>>, state: State) -> Self {
        Self { io, state }
    }
}

impl<T> Future for ReadTask<T>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.state.is_io_shutdown() {
            log::trace!("read task is instructed to shutdown");
            Poll::Ready(())
        } else if self.state.is_io_stop() {
            self.state.dsp_wake_task();
            Poll::Ready(())
        } else if self.state.is_read_paused() {
            self.state.register_read_task(cx.waker());
            Poll::Pending
        } else {
            let mut io = self.io.borrow_mut();
            match self.state.with_read_buf(|buf| read(&mut *io, buf, cx)) {
                Ok(res) => {
                    self.state.update_read_task(res, cx.waker());
                    Poll::Pending
                }
                Err(err) => {
                    self.state.set_io_error(err);
                    Poll::Ready(())
                }
            }
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub(super) enum ReadResult {
    Pending,
    Updated,
    BackPressure,
}

pub(super) fn read<T>(
    io: &mut T,
    buf: &mut BytesMut,
    cx: &mut Context<'_>,
) -> Result<ReadResult, Option<io::Error>>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    // make sure we've got room
    let remaining = buf.capacity() - buf.len();
    if remaining < LW {
        buf.reserve(HW - remaining)
    }

    // read all data from socket
    let mut result = ReadResult::Pending;
    loop {
        match crate::codec::poll_read_buf(Pin::new(&mut *io), cx, buf) {
            Poll::Pending => break,
            Poll::Ready(Ok(n)) => {
                if n == 0 {
                    log::trace!("io stream is disconnected");
                    return Err(None);
                } else {
                    if buf.len() > HW {
                        log::trace!("buffer is too large {}, pause", buf.len());
                        return Ok(ReadResult::BackPressure);
                    }

                    result = ReadResult::Updated;
                }
            }
            Poll::Ready(Err(err)) => {
                log::trace!("read task failed on io {:?}", err);
                return Err(Some(err));
            }
        }
    }

    Ok(result)
}
