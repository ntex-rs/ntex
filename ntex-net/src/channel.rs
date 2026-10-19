//! A one-shot, futures-aware channel.
use std::{cell::Cell, rc::Rc};
use std::{fmt, future::Future, future::poll_fn, io, pin::Pin, task::Context, task::Poll};

use ntex_util::task::LocalWaker;

/// Notified when a receiver is dropped before its value is sent.
pub(crate) trait Cancel {
    fn cancel(&self, id: u32);
}

/// Creates a new futures-aware, one-shot channel.
pub fn create<T>() -> (Sender<T>, Receiver<T>) {
    create_inner(None)
}

/// Creates a channel that calls `hnd.cancel(id)` if the receiver is
/// dropped while the sender is still pending.
pub(crate) fn with_cancel<T>(hnd: Rc<dyn Cancel>, id: u32) -> (Sender<T>, Receiver<T>) {
    create_inner(Some((hnd, id)))
}

fn create_inner<T>(on_cancel: Option<(Rc<dyn Cancel>, u32)>) -> (Sender<T>, Receiver<T>) {
    let inner = Rc::new(Inner {
        value: Cell::new(None),
        rx_task: LocalWaker::new(),
        on_cancel: Cell::new(on_cancel),
    });
    let tx = Sender {
        inner: inner.clone(),
    };
    let rx = Receiver { inner };
    (tx, rx)
}

#[cfg(windows)]
pub(crate) fn create_with_cancel<T>(
    on_cancel: impl FnOnce() + 'static,
) -> (Sender<T>, Receiver<T>) {
    struct Callback<F>(Cell<Option<F>>);

    impl<F: FnOnce()> Cancel for Callback<F> {
        fn cancel(&self, _: u32) {
            if let Some(on_cancel) = self.0.take() {
                on_cancel();
            }
        }
    }

    with_cancel(Rc::new(Callback(Cell::new(Some(on_cancel)))), 0)
}

#[derive(Debug)]
/// Represents the completion half of a oneshot through which the result of a
/// computation is signaled.
pub struct Sender<T> {
    inner: Rc<Inner<T>>,
}

#[derive(Debug)]
/// A future representing the completion of a computation happening elsewhere in
/// memory.
#[must_use = "futures do nothing unless polled"]
pub struct Receiver<T> {
    inner: Rc<Inner<T>>,
}

// The channels do not ever project Pin to the inner T
impl<T> Unpin for Receiver<T> {}
impl<T> Unpin for Sender<T> {}

struct Inner<T> {
    value: Cell<Option<io::Result<T>>>,
    rx_task: LocalWaker,
    on_cancel: Cell<Option<(Rc<dyn Cancel>, u32)>>,
}

impl<T> Sender<T> {
    #[cfg(windows)]
    pub(crate) fn is_closed(&self) -> bool {
        Rc::strong_count(&self.inner) == 1
    }

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
    pub fn send(self, val: io::Result<T>) -> Result<(), io::Result<T>> {
        drop(self.inner.on_cancel.take());

        if Rc::strong_count(&self.inner) == 2 {
            self.inner.value.set(Some(val));
            self.inner.rx_task.wake();
            Ok(())
        } else {
            Err(val)
        }
    }

    /// Returns `true` if the receiver is dropped.
    pub fn is_canceled(&self) -> bool {
        Rc::strong_count(&self.inner) == 1
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        drop(self.inner.on_cancel.take());
        self.inner.rx_task.wake();
    }
}

impl<T> Receiver<T> {
    pub fn new(val: io::Result<T>) -> Self {
        let inner = Rc::new(Inner {
            value: Cell::new(Some(val)),
            rx_task: LocalWaker::new(),
            on_cancel: Cell::new(None),
        });
        Receiver { inner }
    }

    /// Wait until the oneshot is ready and return value
    pub async fn recv(&self) -> io::Result<T> {
        poll_fn(|cx| self.poll_recv(cx)).await
    }

    /// Polls the oneshot to determine if value is ready
    fn poll_recv(&self, cx: &mut Context<'_>) -> Poll<io::Result<T>> {
        // If we've got a value, then skip the logic below as we're done.
        if let Some(val) = self.inner.value.take() {
            return Poll::Ready(val);
        }

        // Check if sender is dropped and return error if it is.
        if Rc::strong_count(&self.inner) == 1 {
            Poll::Ready(Err(io::Error::other("IO Driver is gone")))
        } else {
            self.inner.rx_task.register(cx.waker());
            Poll::Pending
        }
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        if let Some((hnd, id)) = self.inner.on_cancel.take() {
            hnd.cancel(id);
        }
    }
}

impl<T> Future for Receiver<T> {
    type Output = io::Result<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.poll_recv(cx)
    }
}

impl<T: fmt::Debug> fmt::Debug for Inner<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let val = self.value.take();
        let result = f.debug_struct("Inner").field("value", &val).finish();
        self.value.set(val);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn cancellation_only_while_pending() {
        let cancelled = Rc::new(Cell::new(0));
        let channel = || {
            let cancelled = cancelled.clone();
            create_with_cancel::<()>(move || cancelled.set(cancelled.get() + 1))
        };

        let (tx, rx) = channel();
        assert!(!tx.is_closed());
        drop(rx);
        assert!(tx.is_closed());
        assert_eq!(cancelled.get(), 1);
        assert!(tx.send(Ok(())).is_err());
        assert_eq!(cancelled.get(), 1);

        let (tx, rx) = channel();
        tx.send(Ok(())).unwrap();
        drop(rx);
        assert_eq!(cancelled.get(), 1);

        let (tx, rx) = channel();
        tx.send(Err(io::Error::other("connect failed"))).unwrap();
        drop(rx);
        assert_eq!(cancelled.get(), 1);

        let (tx, rx) = channel();
        drop(tx);
        drop(rx);
        assert_eq!(cancelled.get(), 1);
    }

    #[ntex::test]
    async fn test_oneshot() {
        let (tx, rx) = create();
        assert!(format!("{tx:?}").contains("Sender"));
        assert!(format!("{rx:?}").contains("Receiver"));

        tx.send(Ok("test")).unwrap();
        assert_eq!(rx.await.unwrap(), "test");

        let (tx, rx) = create();
        tx.send(Ok("test")).unwrap();
        assert_eq!(rx.recv().await.unwrap(), "test");

        let (tx, rx) = create();
        assert!(!tx.is_canceled());
        drop(rx);
        assert!(tx.is_canceled());
        assert!(tx.send(Ok("test")).is_err());

        let (tx, rx) = create::<&'static str>();
        drop(tx);
        assert!(rx.await.is_err());

        let (tx, rx) = create::<&'static str>();
        tx.send(Ok("test")).unwrap();
        assert_eq!(rx.await.unwrap(), "test");

        let (tx, rx) = create::<&'static str>();
        drop(tx);
        assert!(rx.await.is_err());
    }

    #[ntex::test]
    async fn cancel_hook() {
        struct Hook(Cell<Option<u32>>);
        impl Cancel for Hook {
            fn cancel(&self, id: u32) {
                assert!(self.0.replace(Some(id)).is_none());
            }
        }
        let hook = Rc::new(Hook(Cell::new(None)));

        let (_tx, rx) = with_cancel::<()>(hook.clone(), 7);
        drop(rx);
        assert_eq!(hook.0.take(), Some(7));

        // a sent or abandoned value is not canceled
        let (tx, rx) = with_cancel(hook.clone(), 8);
        tx.send(Ok(())).unwrap();
        drop(rx);
        let (tx, rx) = with_cancel::<()>(hook.clone(), 9);
        drop(tx);
        drop(rx);
        assert_eq!(hook.0.take(), None);
    }
}
