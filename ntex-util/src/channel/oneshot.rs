//! A one-shot, futures-aware channel.
use std::{future::Future, pin::Pin, task::Context, task::Poll};

use super::{cell::Cell, Canceled};
use crate::task::LocalWaker;

/// Creates a new futures-aware, one-shot channel.
pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let inner = Cell::new(Inner {
        value: None,
        rx_task: LocalWaker::new(),
    });
    let tx = Sender {
        inner: inner.clone(),
    };
    let rx = Receiver { inner };
    (tx, rx)
}

/// Represents the completion half of a oneshot through which the result of a
/// computation is signaled.
#[derive(Debug)]
pub struct Sender<T> {
    inner: Cell<Inner<T>>,
}

/// A future representing the completion of a computation happening elsewhere in
/// memory.
#[derive(Debug)]
#[must_use = "futures do nothing unless polled"]
pub struct Receiver<T> {
    inner: Cell<Inner<T>>,
}

// The channels do not ever project Pin to the inner T
impl<T> Unpin for Receiver<T> {}
impl<T> Unpin for Sender<T> {}

#[derive(Debug)]
struct Inner<T> {
    value: Option<T>,
    rx_task: LocalWaker,
}

impl<T> Sender<T> {
    /// Completes this oneshot with a successful result.
    ///
    /// This function will consume `self` and indicate to the other end, the
    /// `Receiver`, that the error provided is the result of the computation this
    /// represents.
    ///
    /// If the value is successfully enqueued for the remote end to receive,
    /// then `Ok(())` is returned. If the receiving end was dropped before
    /// this function was called, however, then `Err` is returned with the value
    /// provided.
    pub fn send(self, val: T) -> Result<(), T> {
        if self.inner.strong_count() == 2 {
            let inner = self.inner.get_mut();
            inner.value = Some(val);
            inner.rx_task.wake();
            Ok(())
        } else {
            Err(val)
        }
    }

    /// Tests to see whether this `Sender`'s corresponding `Receiver`
    /// has gone away.
    pub fn is_canceled(&self) -> bool {
        self.inner.strong_count() == 1
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        self.inner.get_ref().rx_task.wake();
    }
}

impl<T> Future for Receiver<T> {
    type Output = Result<T, Canceled>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        // If we've got a value, then skip the logic below as we're done.
        if let Some(val) = this.inner.get_mut().value.take() {
            return Poll::Ready(Ok(val));
        }

        // Check if sender is dropped and return error if it is.
        if this.inner.strong_count() == 1 {
            Poll::Ready(Err(Canceled))
        } else {
            this.inner.get_ref().rx_task.register(cx.waker());
            Poll::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::future::lazy;

    #[ntex::test]
    async fn test_oneshot() {
        let (tx, rx) = channel();
        tx.send("test").unwrap();
        assert_eq!(rx.await.unwrap(), "test");

        let (tx, rx) = channel();
        assert!(!tx.is_canceled());
        drop(rx);
        assert!(tx.is_canceled());
        assert!(tx.send("test").is_err());

        let (tx, rx) = channel::<&'static str>();
        drop(tx);
        assert!(rx.await.is_err());

        let (tx, mut rx) = channel::<&'static str>();
        assert_eq!(lazy(|cx| Pin::new(&mut rx).poll(cx)).await, Poll::Pending);
        tx.send("test").unwrap();
        assert_eq!(rx.await.unwrap(), "test");

        let (tx, mut rx) = channel::<&'static str>();
        assert_eq!(lazy(|cx| Pin::new(&mut rx).poll(cx)).await, Poll::Pending);
        drop(tx);
        assert!(rx.await.is_err());
    }
}
