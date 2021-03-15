use std::{cell::RefCell, future::Future, pin::Pin, rc::Rc, task::Context, task::Poll};

use crate::codec::{AsyncRead, AsyncWrite};
use crate::framed::State;

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
            self.state.wake_dispatcher();
            Poll::Ready(())
        } else if self.state.is_read_paused() {
            self.state.register_read_task(cx.waker());
            Poll::Pending
        } else {
            let mut io = self.io.borrow_mut();
            if self.state.read_io(&mut *io, cx) {
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        }
    }
}
