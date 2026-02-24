//! A futures-aware bounded(1) channel.
use std::{cell::Cell, fmt, future::poll_fn, task::Context, task::Poll};

use crate::task::LocalWaker;

/// Creates a new futures-aware, channel.
pub fn channel<T>() -> Inplace<T> {
    Inplace {
        value: Cell::new(None),
        rx_task: LocalWaker::new(),
    }
}

/// A futures-aware bounded(1) channel.
pub struct Inplace<T> {
    value: Cell<Option<T>>,
    rx_task: LocalWaker,
}

// The channels do not ever project Pin to the inner T
impl<T> Unpin for Inplace<T> {}

impl<T> fmt::Debug for Inplace<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Inplace<T>")
    }
}

impl<T> Inplace<T> {
    /// Set a successful result.
    ///
    /// If the value is successfully enqueued to be received, then `Ok(())` is
    /// returned. If the previous value has not been consumed yet, then `Err` is
    /// returned with the value provided.
    pub fn send(&self, val: T) -> Result<(), T> {
        if let Some(v) = self.value.take() {
            self.value.set(Some(v));
            Err(val)
        } else {
            self.value.set(Some(val));
            self.rx_task.wake();
            Ok(())
        }
    }

    /// Wait until the oneshot is ready and return value
    pub async fn recv(&self) -> T {
        poll_fn(|cx| self.poll_recv(cx)).await
    }

    /// Polls the oneshot to determine if value is ready
    pub fn poll_recv(&self, cx: &mut Context<'_>) -> Poll<T> {
        // If we've got a value, then skip the logic below as we're done.
        if let Some(val) = self.value.take() {
            return Poll::Ready(val);
        }

        // Check if sender is dropped and return error if it is.
        self.rx_task.register(cx.waker());
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::future::lazy;

    #[ntex::test]
    async fn test_inplace() {
        let ch = channel();
        assert_eq!(lazy(|cx| ch.poll_recv(cx)).await, Poll::Pending);

        assert!(ch.send(1).is_ok());
        assert!(ch.send(2) == Err(2));
        assert_eq!(lazy(|cx| ch.poll_recv(cx)).await, Poll::Ready(1));

        assert!(ch.send(1).is_ok());
        assert_eq!(ch.recv().await, 1);
        assert!(format!("{ch:?}").contains("Inplace"));
    }
}
