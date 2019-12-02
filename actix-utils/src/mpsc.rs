//! A multi-producer, single-consumer, futures-aware, FIFO queue with back
//! pressure, for use communicating between tasks on the same thread.
//!
//! These queues are the same as those in `futures::sync`, except they're not
//! intended to be sent across threads.

use std::any::Any;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::error::Error;
use std::pin::Pin;
use std::rc::{Rc, Weak};
use std::task::{Context, Poll};
use std::{fmt, mem};

use futures::{Sink, Stream};

use crate::task::LocalWaker;

/// Creates a unbounded in-memory channel with buffered storage.
pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let shared = Rc::new(RefCell::new(Shared {
        buffer: VecDeque::new(),
        blocked_recv: LocalWaker::new(),
    }));
    let sender = Sender {
        shared: Rc::downgrade(&shared),
    };
    let receiver = Receiver {
        state: State::Open(shared),
    };
    (sender, receiver)
}

#[derive(Debug)]
struct Shared<T> {
    buffer: VecDeque<T>,
    blocked_recv: LocalWaker,
}

/// The transmission end of a channel.
///
/// This is created by the `channel` function.
#[derive(Debug)]
pub struct Sender<T> {
    shared: Weak<RefCell<Shared<T>>>,
}

impl<T> Sender<T> {
    /// Sends the provided message along this channel.
    pub fn send(&self, item: T) -> Result<(), SendError<T>> {
        let shared = match self.shared.upgrade() {
            Some(shared) => shared,
            None => return Err(SendError(item)), // receiver was dropped
        };
        let mut shared = shared.borrow_mut();

        shared.buffer.push_back(item);
        shared.blocked_recv.wake();
        Ok(())
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Sender {
            shared: self.shared.clone(),
        }
    }
}

impl<T> Sink<T> for Sender<T> {
    type Error = SendError<T>;

    fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, item: T) -> Result<(), SendError<T>> {
        self.send(item)
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), SendError<T>>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let shared = match self.shared.upgrade() {
            Some(shared) => shared,
            None => return,
        };
        // The number of existing `Weak` indicates if we are possibly the last
        // `Sender`. If we are the last, we possibly must notify a blocked
        // `Receiver`. `self.shared` is always one of the `Weak` to this shared
        // data. Therefore the smallest possible Rc::weak_count(&shared) is 1.
        if Rc::weak_count(&shared) == 1 {
            // Wake up receiver as its stream has ended
            shared.borrow_mut().blocked_recv.wake();
        }
    }
}

/// The receiving end of a channel which implements the `Stream` trait.
///
/// This is created by the `channel` function.
#[derive(Debug)]
pub struct Receiver<T> {
    state: State<T>,
}

impl<T> Unpin for Receiver<T> {}

/// Possible states of a receiver. We're either Open (can receive more messages)
/// or we're closed with a list of messages we have left to receive.
#[derive(Debug)]
enum State<T> {
    Open(Rc<RefCell<Shared<T>>>),
    Closed(VecDeque<T>),
}

impl<T> Receiver<T> {
    /// Closes the receiving half
    ///
    /// This prevents any further messages from being sent on the channel while
    /// still enabling the receiver to drain messages that are buffered.
    pub fn close(&mut self) {
        let items = match self.state {
            State::Open(ref state) => {
                let mut state = state.borrow_mut();
                mem::replace(&mut state.buffer, VecDeque::new())
            }
            State::Closed(_) => return,
        };
        self.state = State::Closed(items);
    }
}

impl<T> Stream for Receiver<T> {
    type Item = T;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let me = match self.state {
            State::Open(ref mut me) => me,
            State::Closed(ref mut items) => return Poll::Ready(items.pop_front()),
        };

        if let Some(shared) = Rc::get_mut(me) {
            // All senders have been dropped, so drain the buffer and end the
            // stream.
            return Poll::Ready(shared.borrow_mut().buffer.pop_front());
        }

        let mut shared = me.borrow_mut();
        if let Some(msg) = shared.buffer.pop_front() {
            Poll::Ready(Some(msg))
        } else {
            shared.blocked_recv.register(cx.waker());
            Poll::Pending
        }
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.close();
    }
}

/// Error type for sending, used when the receiving end of a channel is
/// dropped
pub struct SendError<T>(T);

impl<T> fmt::Debug for SendError<T> {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_tuple("SendError").field(&"...").finish()
    }
}

impl<T> fmt::Display for SendError<T> {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(fmt, "send failed because receiver is gone")
    }
}

impl<T: Any> Error for SendError<T> {
    fn description(&self) -> &str {
        "send failed because receiver is gone"
    }
}

impl<T> SendError<T> {
    /// Returns the message that was attempted to be sent but failed.
    pub fn into_inner(self) -> T {
        self.0
    }
}
