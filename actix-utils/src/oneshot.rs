//! A one-shot, futures-aware channel
//!
//! This channel is similar to that in `sync::oneshot` but cannot be sent across
//! threads.

use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::{Rc, Weak};
use std::task::{Context, Poll};

pub use futures::channel::oneshot::Canceled;

use crate::task::LocalWaker;

/// Creates a new futures-aware, one-shot channel.
///
/// This function is the same as `sync::oneshot::channel` except that the
/// returned values cannot be sent across threads.
pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let inner = Rc::new(RefCell::new(Inner {
        value: None,
        tx_task: LocalWaker::new(),
        rx_task: LocalWaker::new(),
    }));
    let tx = Sender {
        inner: Rc::downgrade(&inner),
    };
    let rx = Receiver {
        state: State::Open(inner),
    };
    (tx, rx)
}

/// Represents the completion half of a oneshot through which the result of a
/// computation is signaled.
///
/// This is created by the `unsync::oneshot::channel` function and is equivalent
/// in functionality to `sync::oneshot::Sender` except that it cannot be sent
/// across threads.
#[derive(Debug)]
pub struct Sender<T> {
    inner: Weak<RefCell<Inner<T>>>,
}

/// A future representing the completion of a computation happening elsewhere in
/// memory.
///
/// This is created by the `unsync::oneshot::channel` function and is equivalent
/// in functionality to `sync::oneshot::Receiver` except that it cannot be sent
/// across threads.
#[derive(Debug)]
#[must_use = "futures do nothing unless polled"]
pub struct Receiver<T> {
    state: State<T>,
}

// The channels do not ever project Pin to the inner T
impl<T> Unpin for Receiver<T> {}
impl<T> Unpin for Sender<T> {}

#[derive(Debug)]
enum State<T> {
    Open(Rc<RefCell<Inner<T>>>),
    Closed(Option<T>),
}

#[derive(Debug)]
struct Inner<T> {
    value: Option<T>,
    tx_task: LocalWaker,
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
    /// then `Ok(())` is returned. If the receiving end was deallocated before
    /// this function was called, however, then `Err` is returned with the value
    /// provided.
    pub fn send(self, val: T) -> Result<(), T> {
        if let Some(inner) = self.inner.upgrade() {
            inner.borrow_mut().value = Some(val);
            Ok(())
        } else {
            Err(val)
        }
    }

    /// Polls this `Sender` half to detect whether the `Receiver` this has
    /// paired with has gone away.
    ///
    /// This function can be used to learn about when the `Receiver` (consumer)
    /// half has gone away and nothing will be able to receive a message sent
    /// from `complete`.
    ///
    /// Like `Future::poll`, this function will panic if it's not called from
    /// within the context of a task. In other words, this should only ever be
    /// called from inside another future.
    ///
    /// If `Ready` is returned then it means that the `Receiver` has disappeared
    /// and the result this `Sender` would otherwise produce should no longer
    /// be produced.
    ///
    /// If `NotReady` is returned then the `Receiver` is still alive and may be
    /// able to receive a message if sent. The current task, however, is
    /// scheduled to receive a notification if the corresponding `Receiver` goes
    /// away.
    pub fn poll_canceled(&mut self, cx: &mut Context) -> Poll<()> {
        match self.inner.upgrade() {
            Some(inner) => {
                inner.borrow_mut().tx_task.register(cx.waker());
                Poll::Pending
            }
            None => Poll::Ready(()),
        }
    }

    /// Tests to see whether this `Sender`'s corresponding `Receiver`
    /// has gone away.
    ///
    /// This function can be used to learn about when the `Receiver` (consumer)
    /// half has gone away and nothing will be able to receive a message sent
    /// from `send`.
    ///
    /// Note that this function is intended to *not* be used in the context of a
    /// future. If you're implementing a future you probably want to call the
    /// `poll_cancel` function which will block the current task if the
    /// cancellation hasn't happened yet. This can be useful when working on a
    /// non-futures related thread, though, which would otherwise panic if
    /// `poll_cancel` were called.
    pub fn is_canceled(&self) -> bool {
        !self.inner.upgrade().is_some()
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let inner = match self.inner.upgrade() {
            Some(inner) => inner,
            None => return,
        };
        inner.borrow().rx_task.wake();
    }
}

impl<T> Receiver<T> {
    /// Gracefully close this receiver, preventing sending any future messages.
    ///
    /// Any `send` operation which happens after this method returns is
    /// guaranteed to fail. Once this method is called the normal `poll` method
    /// can be used to determine whether a message was actually sent or not. If
    /// `Canceled` is returned from `poll` then no message was sent.
    pub fn close(&mut self) {
        match self.state {
            State::Open(ref inner) => {
                let mut inner = inner.borrow_mut();
                inner.tx_task.wake();
                let value = inner.value.take();
                drop(inner);

                self.state = State::Closed(value);
            }
            State::Closed(_) => return,
        };
    }
}

impl<T> Future for Receiver<T> {
    type Output = Result<T, Canceled>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        let inner = match this.state {
            State::Open(ref mut inner) => inner,
            State::Closed(ref mut item) => match item.take() {
                Some(item) => return Poll::Ready(Ok(item.into())),
                None => return Poll::Ready(Err(Canceled)),
            },
        };

        // If we've got a value, then skip the logic below as we're done.
        if let Some(val) = inner.borrow_mut().value.take() {
            return Poll::Ready(Ok(val));
        }

        // If we can get mutable access, then the sender has gone away. We
        // didn't see a value above, so we're canceled. Otherwise we park
        // our task and wait for a value to come in.
        if Rc::get_mut(inner).is_some() {
            Poll::Ready(Err(Canceled))
        } else {
            inner.borrow().rx_task.register(cx.waker());
            Poll::Pending
        }
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.close();
    }
}
