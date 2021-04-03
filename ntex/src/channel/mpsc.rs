//! A multi-producer, single-consumer, futures-aware, FIFO queue.
use std::{collections::VecDeque, fmt, pin::Pin, task::Context, task::Poll};

use futures_core::Stream;
use futures_sink::Sink;

use super::cell::{Cell, WeakCell};
use crate::task::LocalWaker;

/// Creates a unbounded in-memory channel with buffered storage.
pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let shared = Cell::new(Shared {
        has_receiver: true,
        buffer: VecDeque::new(),
        blocked_recv: LocalWaker::new(),
    });
    let sender = Sender {
        shared: shared.clone(),
    };
    let receiver = Receiver { shared };
    (sender, receiver)
}

#[derive(Debug)]
struct Shared<T> {
    buffer: VecDeque<T>,
    blocked_recv: LocalWaker,
    has_receiver: bool,
}

/// The transmission end of a channel.
///
/// This is created by the `channel` function.
#[derive(Debug)]
pub struct Sender<T> {
    shared: Cell<Shared<T>>,
}

impl<T> Unpin for Sender<T> {}

impl<T> Sender<T> {
    /// Sends the provided message along this channel.
    pub fn send(&self, item: T) -> Result<(), SendError<T>> {
        let shared = self.shared.get_mut();
        if !shared.has_receiver {
            return Err(SendError(item)); // receiver was dropped
        };
        shared.buffer.push_back(item);
        shared.blocked_recv.wake();
        Ok(())
    }

    /// Closes the sender half
    ///
    /// This prevents any further messages from being sent on the channel while
    /// still enabling the receiver to drain messages that are buffered.
    pub fn close(&self) {
        let shared = self.shared.get_mut();
        shared.has_receiver = false;
        shared.blocked_recv.wake();
    }

    /// Returns whether this channel is closed without needing a context.
    pub fn is_closed(&self) -> bool {
        self.shared.strong_count() == 1 || !self.shared.get_ref().has_receiver
    }

    /// Returns downgraded sender
    pub fn downgrade(self) -> WeakSender<T> {
        WeakSender {
            shared: self.shared.downgrade(),
        }
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

    fn poll_ready(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, item: T) -> Result<(), SendError<T>> {
        self.send(item)
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Result<(), SendError<T>>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        self.close();
        Poll::Ready(Ok(()))
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let count = self.shared.strong_count();
        let shared = self.shared.get_mut();

        // check is last sender is about to drop
        if shared.has_receiver && count == 2 {
            // Wake up receiver as its stream has ended
            shared.blocked_recv.wake();
        }
    }
}

/// Weak sender type
pub struct WeakSender<T> {
    shared: WeakCell<Shared<T>>,
}

impl<T> WeakSender<T> {
    /// Upgrade to Sender<T>
    pub fn upgrade(&self) -> Option<Sender<T>> {
        self.shared.upgrade().map(|shared| Sender { shared })
    }
}

/// The receiving end of a channel which implements the `Stream` trait.
///
/// This is created by the `channel` function.
#[derive(Debug)]
pub struct Receiver<T> {
    shared: Cell<Shared<T>>,
}

impl<T> Receiver<T> {
    /// Create a Sender
    pub fn sender(&self) -> Sender<T> {
        Sender {
            shared: self.shared.clone(),
        }
    }

    /// Closes the receiving half of a channel, without dropping it.
    ///
    /// This prevents any further messages from being sent on the channel
    /// while still enabling the receiver to drain messages that are buffered.
    pub fn close(&self) {
        self.shared.get_mut().has_receiver = false;
    }

    /// Returns whether this channel is closed without needing a context.
    pub fn is_closed(&self) -> bool {
        self.shared.strong_count() == 1 || !self.shared.get_ref().has_receiver
    }
}

impl<T> Unpin for Receiver<T> {}

impl<T> Stream for Receiver<T> {
    type Item = T;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        let shared = self.shared.get_mut();

        if let Some(msg) = shared.buffer.pop_front() {
            Poll::Ready(Some(msg))
        } else if shared.has_receiver {
            shared.blocked_recv.register(cx.waker());
            if self.shared.strong_count() == 1 {
                // All senders have been dropped, so drain the buffer and end the
                // stream.
                Poll::Ready(None)
            } else {
                Poll::Pending
            }
        } else {
            Poll::Ready(None)
        }
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        let shared = self.shared.get_mut();
        shared.buffer.clear();
        shared.has_receiver = false;
    }
}

/// Error type for sending, used when the receiving end of a channel is
/// dropped
pub struct SendError<T>(T);

impl<T> std::error::Error for SendError<T> {}

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

impl<T> SendError<T> {
    /// Returns the message that was attempted to be sent but failed.
    pub fn into_inner(self) -> T {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{util::lazy, util::next, Stream};
    use futures_sink::Sink;

    #[crate::rt_test]
    async fn test_mpsc() {
        let (tx, mut rx) = channel();
        assert!(format!("{:?}", tx).contains("Sender"));
        assert!(format!("{:?}", rx).contains("Receiver"));

        tx.send("test").unwrap();
        assert_eq!(next(&mut rx).await.unwrap(), "test");

        let tx2 = tx.clone();
        tx2.send("test2").unwrap();
        assert_eq!(next(&mut rx).await.unwrap(), "test2");

        assert_eq!(
            lazy(|cx| Pin::new(&mut rx).poll_next(cx)).await,
            Poll::Pending
        );
        drop(tx2);
        assert_eq!(
            lazy(|cx| Pin::new(&mut rx).poll_next(cx)).await,
            Poll::Pending
        );
        drop(tx);

        let (tx, mut rx) = channel::<String>();
        tx.close();
        assert_eq!(next(&mut rx).await, None);

        let (tx, _rx) = channel::<String>();
        let weak_tx = tx.downgrade();
        assert!(weak_tx.upgrade().is_some());

        let (tx, rx) = channel();
        tx.send("test").unwrap();
        drop(rx);
        assert!(tx.send("test").is_err());

        let (tx, _) = channel();
        let tx2 = tx.clone();
        tx.close();
        assert!(tx.send("test").is_err());
        assert!(tx2.send("test").is_err());

        let err = SendError("test");
        assert!(format!("{:?}", err).contains("SendError"));
        assert!(format!("{}", err).contains("send failed because receiver is gone"));
        assert_eq!(err.into_inner(), "test");
    }

    #[crate::rt_test]
    async fn test_sink() {
        let (mut tx, mut rx) = channel();
        lazy(|cx| {
            assert!(Pin::new(&mut tx).poll_ready(cx).is_ready());
            assert!(Pin::new(&mut tx).start_send("test").is_ok());
            assert!(Pin::new(&mut tx).poll_flush(cx).is_ready());
            assert!(Pin::new(&mut tx).poll_close(cx).is_ready());
        })
        .await;
        assert_eq!(next(&mut rx).await.unwrap(), "test");
        assert_eq!(next(&mut rx).await, None);
    }

    #[crate::rt_test]
    async fn test_close() {
        let (tx, rx) = channel::<()>();
        assert!(!tx.is_closed());
        assert!(!rx.is_closed());

        tx.close();
        assert!(tx.is_closed());
        assert!(rx.is_closed());

        let (tx, rx) = channel::<()>();
        rx.close();
        assert!(tx.is_closed());
    }
}
