//! A one-shot pool, futures-aware channel.
use slab::Slab;
use std::{future::Future, pin::Pin, task::Context, task::Poll};

use super::{cell::Cell, Canceled};
use crate::task::LocalWaker;

/// Creates a new futures-aware, pool of one-shot's.
pub fn new<T>() -> Pool<T> {
    Pool(Cell::new(Slab::new()))
}

/// Futures-aware, pool of one-shot's.
pub struct Pool<T>(Cell<Slab<Inner<T>>>);

bitflags::bitflags! {
    struct Flags: u8 {
        const SENDER = 0b0000_0001;
        const RECEIVER = 0b0000_0010;
    }
}

#[derive(Debug)]
struct Inner<T> {
    flags: Flags,
    value: Option<T>,
    tx_waker: LocalWaker,
    rx_waker: LocalWaker,
}

impl<T> Pool<T> {
    /// Create a new one-shot channel.
    pub fn channel(&self) -> (Sender<T>, Receiver<T>) {
        let token = self.0.get_mut().insert(Inner {
            flags: Flags::all(),
            value: None,
            tx_waker: LocalWaker::default(),
            rx_waker: LocalWaker::default(),
        });

        (
            Sender {
                token,
                inner: self.0.clone(),
            },
            Receiver {
                token,
                inner: self.0.clone(),
            },
        )
    }
}

impl<T> Clone for Pool<T> {
    fn clone(&self) -> Self {
        Pool(self.0.clone())
    }
}

/// Represents the completion half of a oneshot through which the result of a
/// computation is signaled.
#[derive(Debug)]
pub struct Sender<T> {
    token: usize,
    inner: Cell<Slab<Inner<T>>>,
}

/// A future representing the completion of a computation happening elsewhere in
/// memory.
#[derive(Debug)]
#[must_use = "futures do nothing unless polled"]
pub struct Receiver<T> {
    token: usize,
    inner: Cell<Slab<Inner<T>>>,
}

#[allow(clippy::mut_from_ref)]
fn get_inner<T>(inner: &Cell<Slab<Inner<T>>>, token: usize) -> &mut Inner<T> {
    unsafe { inner.get_mut().get_unchecked_mut(token) }
}

// The oneshots do not ever project Pin to the inner T
impl<T> Unpin for Receiver<T> {}
impl<T> Unpin for Sender<T> {}

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
        let inner = get_inner(&self.inner, self.token);
        if inner.flags.contains(Flags::RECEIVER) {
            inner.value = Some(val);
            inner.rx_waker.wake();
            Ok(())
        } else {
            Err(val)
        }
    }

    /// Tests to see whether this `Sender`'s corresponding `Receiver`
    /// has gone away.
    pub fn is_canceled(&self) -> bool {
        !get_inner(&self.inner, self.token)
            .flags
            .contains(Flags::RECEIVER)
    }

    /// Polls the channel to determine if receiving path is dropped
    pub fn poll_canceled(&self, cx: &mut Context<'_>) -> Poll<()> {
        let inner = get_inner(&self.inner, self.token);

        if inner.flags.contains(Flags::RECEIVER) {
            inner.tx_waker.register(cx.waker());
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let inner = get_inner(&self.inner, self.token);
        if inner.flags.contains(Flags::RECEIVER) {
            inner.rx_waker.wake();
            inner.flags.remove(Flags::SENDER);
        } else {
            self.inner.get_mut().remove(self.token);
        }
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        let inner = get_inner(&self.inner, self.token);
        if inner.flags.contains(Flags::SENDER) {
            inner.tx_waker.wake();
            inner.flags.remove(Flags::RECEIVER);
        } else {
            self.inner.get_mut().remove(self.token);
        }
    }
}

impl<T> Future for Receiver<T> {
    type Output = Result<T, Canceled>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let inner = get_inner(&this.inner, this.token);

        // If we've got a value, then skip the logic below as we're done.
        if let Some(val) = inner.value.take() {
            return Poll::Ready(Ok(val));
        }

        // Check if sender is dropped and return error if it is.
        if !inner.flags.contains(Flags::SENDER) {
            Poll::Ready(Err(Canceled))
        } else {
            inner.rx_waker.register(cx.waker());
            Poll::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::future::lazy;

    #[ntex::test]
    async fn test_pool() {
        let p = new();
        let (tx, rx) = p.channel();
        tx.send("test").unwrap();
        assert_eq!(rx.await.unwrap(), "test");

        let p2 = p.clone();
        let (tx, rx) = p2.channel();
        assert!(!tx.is_canceled());
        drop(rx);
        assert!(tx.is_canceled());
        assert!(tx.send("test").is_err());

        let (tx, rx) = new::<&'static str>().channel();
        drop(tx);
        assert!(rx.await.is_err());

        let (tx, mut rx) = new::<&'static str>().channel();
        assert_eq!(lazy(|cx| Pin::new(&mut rx).poll(cx)).await, Poll::Pending);
        tx.send("test").unwrap();
        assert_eq!(rx.await.unwrap(), "test");

        let (tx, mut rx) = new::<&'static str>().channel();
        assert_eq!(lazy(|cx| Pin::new(&mut rx).poll(cx)).await, Poll::Pending);
        drop(tx);
        assert!(rx.await.is_err());

        let (mut tx, rx) = new::<&'static str>().channel();
        assert!(!tx.is_canceled());
        assert_eq!(
            lazy(|cx| Pin::new(&mut tx).poll_canceled(cx)).await,
            Poll::Pending
        );
        drop(rx);
        assert!(tx.is_canceled());
        assert_eq!(
            lazy(|cx| Pin::new(&mut tx).poll_canceled(cx)).await,
            Poll::Ready(())
        );
    }
}
