//! A one-shot pool, futures-aware channel.
use slab::Slab;
use std::{fmt, future::Future, pin::Pin, task::Context, task::Poll};

use super::{Canceled, cell::Cell, cell::wake};
use crate::task::LocalWaker;

/// Creates a new futures-aware pool of one-shot channels.
pub fn new<T>() -> Pool<T> {
    Pool(Cell::new(Slab::new()))
}

/// A futures-aware pool of one-shot channels.
pub struct Pool<T>(Cell<Slab<Inner<T>>>);

impl<T> fmt::Debug for Pool<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pool")
            .field("size", &self.0.get_ref().len())
            .finish()
    }
}

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
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

impl<T> Default for Pool<T> {
    fn default() -> Pool<T> {
        new()
    }
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

    /// Shrinks the capacity of the pool as much as possible.
    pub fn shrink_to_fit(&self) {
        self.0.get_mut().shrink_to_fit();
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

// A channel's slot stays occupied while its sender or receiver is alive.
// Slot references must not be held across user code, which may insert into
// the slab and reallocate it, see `Cell`.
#[allow(clippy::mut_from_ref)]
fn get_inner<T>(inner: &Cell<Slab<Inner<T>>>, token: usize) -> &mut Inner<T> {
    // SAFETY: The live channel half keeps `token` occupied; callers exclude overlapping slot borrows.
    unsafe { inner.get_mut().get_unchecked_mut(token) }
}

fn get_inner_ref<T>(inner: &Cell<Slab<Inner<T>>>, token: usize) -> &Inner<T> {
    // SAFETY: The live channel half keeps `token` occupied until its drop removes the slot.
    unsafe { inner.get_ref().get_unchecked(token) }
}

/// Removes the channel slot, dropping its contents after the borrow ends.
fn remove<T>(inner: &Cell<Slab<Inner<T>>>, token: usize) {
    let removed = inner.get_mut().remove(token);
    drop(removed);
}

// The oneshots do not ever project Pin to the inner T
impl<T> Unpin for Receiver<T> {}
impl<T> Unpin for Sender<T> {}

impl<T> Sender<T> {
    /// Completes this oneshot with a successful result.
    ///
    /// This function will consume `self` and indicate to the other end, the
    /// `Receiver`, that the value provided is the result of the computation this
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
            let waker = inner.rx_waker.take_local();
            wake(waker);
            Ok(())
        } else {
            Err(val)
        }
    }

    /// Tests to see whether this `Sender`'s corresponding `Receiver`
    /// has gone away.
    pub fn is_canceled(&self) -> bool {
        !get_inner_ref(&self.inner, self.token)
            .flags
            .contains(Flags::RECEIVER)
    }

    /// Polls the channel to determine if receiving path is dropped
    pub fn poll_canceled(&self, cx: &mut Context<'_>) -> Poll<()> {
        if self.is_canceled() {
            return Poll::Ready(());
        }
        LocalWaker::register_in(
            || &get_inner_ref(&self.inner, self.token).tx_waker,
            cx.waker(),
        );

        // the waker callbacks may have dropped the receiver
        if self.is_canceled() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let inner = get_inner(&self.inner, self.token);
        if inner.flags.contains(Flags::RECEIVER) {
            inner.flags.remove(Flags::SENDER);
            let waker = inner.rx_waker.take_local();
            wake(waker);
        } else {
            remove(&self.inner, self.token);
        }
    }
}

impl<T> Receiver<T> {
    /// Polls for the value.
    ///
    /// Returns [`Canceled`] if the sender is dropped without sending a value.
    pub fn poll_recv(&self, cx: &mut Context<'_>) -> Poll<Result<T, Canceled>> {
        if let Poll::Ready(res) = self.try_recv() {
            return Poll::Ready(res);
        }
        LocalWaker::register_in(
            || &get_inner_ref(&self.inner, self.token).rx_waker,
            cx.waker(),
        );

        // the waker callbacks may have sent the value or dropped the sender
        self.try_recv()
    }

    fn try_recv(&self) -> Poll<Result<T, Canceled>> {
        let inner = get_inner(&self.inner, self.token);
        if let Some(val) = inner.value.take() {
            Poll::Ready(Ok(val))
        } else if inner.flags.contains(Flags::SENDER) {
            Poll::Pending
        } else {
            Poll::Ready(Err(Canceled))
        }
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        let inner = get_inner(&self.inner, self.token);
        if inner.flags.contains(Flags::SENDER) {
            inner.flags.remove(Flags::RECEIVER);
            let waker = inner.tx_waker.take_local();
            wake(waker);
        } else {
            remove(&self.inner, self.token);
        }
    }
}

impl<T> Future for Receiver<T> {
    type Output = Result<T, Canceled>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.poll_recv(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::future::lazy;
    use crate::task::hooks::{ON_WAKE, hook_waker};

    /// STATEMENT: Pooled one-shot channels preserve delivery and cancellation behavior.
    ///
    /// HOW:
    /// - Use new, cloned, and default pools to deliver values and inspect debug representations.
    /// - Poll before send or endpoint drop and verify receiver errors and sender cancellation state.
    #[ntex::test]
    async fn test_pool() {
        let p = new();
        p.shrink_to_fit();
        assert!(format!("{p:?}").contains("Pool"));

        let (tx, rx) = p.channel();
        assert!(format!("{tx:?}").contains("Sender"));
        assert!(format!("{rx:?}").contains("Receiver"));

        tx.send("test").unwrap();
        assert_eq!(rx.await.unwrap(), "test");
        assert!(format!("{Canceled}").contains("canceled"));
        assert!(format!("{Canceled:?}").contains("Canceled"));

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

        let p = Pool::default();
        let (tx, rx) = p.channel();
        tx.send("test").unwrap();
        assert_eq!(rx.await.unwrap(), "test");
    }

    /// STATEMENT: Pooled channels access and release their shared slab slots without
    /// undefined behavior.
    ///
    /// HOW:
    /// - Poll a receiver and sender pending with `Waker::noop`, send a `String`, and receive it.
    /// - Drop a sender before sending and check the receiver gets `Canceled`.
    /// - Drop a receiver and check the sender is canceled and `send` returns the value.
    /// - Drop the remaining receiver and check the pool reports no occupied slots.
    #[test]
    fn miri_pool_channels_share_slab_slots() {
        let mut cx = Context::from_waker(std::task::Waker::noop());
        let pool = new::<String>();

        let (tx, rx) = pool.channel();
        assert!(rx.poll_recv(&mut cx).is_pending());
        assert!(tx.poll_canceled(&mut cx).is_pending());
        assert!(!tx.is_canceled());
        tx.send("a".to_string()).unwrap();
        assert_eq!(rx.poll_recv(&mut cx), Poll::Ready(Ok("a".to_string())));
        drop(rx);

        let (tx, rx) = pool.channel();
        drop(tx);
        assert_eq!(rx.poll_recv(&mut cx), Poll::Ready(Err(Canceled)));

        let (tx2, rx2) = pool.channel();
        drop(rx2);
        assert!(tx2.is_canceled());
        assert!(tx2.poll_canceled(&mut cx).is_ready());
        assert_eq!(tx2.send("b".to_string()), Err("b".to_string()));

        drop(rx);
        assert!(format!("{pool:?}").contains("size: 0"));
        pool.shrink_to_fit();
    }

    /// STATEMENT: Dropping a sender is sound when the receiver's wake callback drops the receiver and grows the pool.
    ///
    /// HOW:
    /// - Register the receiver with `hook_waker`, then move it into an `ON_WAKE` hook that drops it and creates 64 channels.
    /// - Drop the sender and check the pool holds only the 64 new channels.
    /// - Check a new channel still delivers a value.
    #[test]
    fn miri_sender_drop_with_reentrant_wake() {
        let pool = new::<usize>();
        let (tx, rx) = pool.channel();
        let waker = hook_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(rx.poll_recv(&mut cx).is_pending());

        let extra = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let (p, e) = (pool.clone(), extra.clone());
        ON_WAKE.set(Some(Box::new(move || {
            drop(rx);
            e.borrow_mut().extend((0..64).map(|_| p.channel()));
        })));
        drop(tx);

        assert_eq!(extra.borrow().len(), 64);
        assert!(format!("{pool:?}").contains("size: 64"));

        let (tx, rx) = pool.channel();
        tx.send(5).unwrap();
        assert_eq!(rx.poll_recv(&mut cx), Poll::Ready(Ok(5)));
    }
}
