//! Bytes stream
use std::cell::{Cell, RefCell};
use std::task::{Context, Poll};
use std::{collections::VecDeque, fmt, future::poll_fn, pin::Pin, rc::Rc, rc::Weak};

use ntex_bytes::Bytes;

use crate::{task::LocalWaker, Stream};

/// max buffer size 32k
const MAX_BUFFER_SIZE: usize = 32_768;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// Stream is ready
    Read,
    /// Stream is paused
    Pause,
    /// Receiver side is dropped
    Dropped,
}

/// Create bytes stream.
///
/// This method construct two objects responsible for bytes stream
/// generation.
pub fn channel<E>() -> (Sender<E>, Receiver<E>) {
    let inner = Rc::new(Inner::new(false));

    (
        Sender {
            inner: Rc::downgrade(&inner),
        },
        Receiver { inner },
    )
}

/// Create closed bytes stream.
///
/// This method construct two objects responsible for bytes stream
/// generation.
pub fn eof<E>() -> (Sender<E>, Receiver<E>) {
    let inner = Rc::new(Inner::new(true));

    (
        Sender {
            inner: Rc::downgrade(&inner),
        },
        Receiver { inner },
    )
}

/// Create empty stream
pub fn empty<E>(data: Option<Bytes>) -> Receiver<E> {
    let rx = Receiver {
        inner: Rc::new(Inner::new(true)),
    };
    if let Some(data) = data {
        rx.put(data);
    }
    rx
}

/// Buffered stream of byte chunks
///
/// Payload stores chunks in a vector. Chunks can be received with
/// `.read()` method.
#[derive(Debug)]
pub struct Receiver<E> {
    inner: Rc<Inner<E>>,
}

impl<E> Receiver<E> {
    /// Set max stream size
    ///
    /// By default max buffer size is set to 32Kb
    #[inline]
    pub fn max_size(&self, size: usize) {
        self.inner.max_size.set(size);
    }

    /// Put unused data back to stream
    #[inline]
    pub fn put(&self, data: Bytes) {
        self.inner.unread_data(data);
    }

    #[inline]
    /// Read next available bytes chunk
    pub async fn read(&self) -> Option<Result<Bytes, E>> {
        poll_fn(|cx| self.poll_read(cx)).await
    }

    #[inline]
    pub fn poll_read(&self, cx: &mut Context<'_>) -> Poll<Option<Result<Bytes, E>>> {
        if let Some(data) = self.inner.items.borrow_mut().pop_front() {
            let len = self.inner.len.get() - data.len();
            self.inner.len.set(len);
            let need_read = if len < self.inner.max_size.get() {
                self.inner.insert_flag(Flags::NEED_READ);
                true
            } else {
                self.inner.remove_flag(Flags::NEED_READ);
                false
            };
            if need_read {
                self.inner.rx_task.register(cx.waker());
                self.inner.tx_task.wake();
            }
            Poll::Ready(Some(Ok(data)))
        } else if let Some(err) = self.inner.err.take() {
            Poll::Ready(Some(Err(err)))
        } else if self.inner.flags.get().intersects(Flags::EOF | Flags::ERROR) {
            Poll::Ready(None)
        } else {
            self.inner.insert_flag(Flags::NEED_READ);
            self.inner.rx_task.register(cx.waker());
            self.inner.tx_task.wake();
            Poll::Pending
        }
    }
}

impl<E> Stream for Receiver<E> {
    type Item = Result<Bytes, E>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, E>>> {
        self.poll_read(cx)
    }
}

/// Sender part of the payload stream
#[derive(Debug)]
pub struct Sender<E> {
    inner: Weak<Inner<E>>,
}

impl<E> Drop for Sender<E> {
    fn drop(&mut self) {
        if let Some(shared) = self.inner.upgrade() {
            shared.insert_flag(Flags::EOF);
        }
    }
}

impl<E> Sender<E> {
    /// Set stream error
    pub fn set_error(&self, err: E) {
        if let Some(shared) = self.inner.upgrade() {
            shared.set_error(err);
        }
    }

    /// Set stream eof
    pub fn feed_eof(&self) {
        if let Some(shared) = self.inner.upgrade() {
            shared.feed_eof();
        }
    }

    /// Add chunk to the stream
    pub fn feed_data(&self, data: Bytes) {
        if let Some(shared) = self.inner.upgrade() {
            shared.feed_data(data)
        }
    }

    /// Check stream readiness
    pub async fn ready(&self) -> Status {
        poll_fn(|cx| self.poll_ready(cx)).await
    }

    /// Check stream readiness
    pub fn poll_ready(&self, cx: &mut Context<'_>) -> Status {
        // we check only if Payload (other side) is alive,
        // otherwise always return true (consume payload)
        if let Some(shared) = self.inner.upgrade() {
            if shared.flags.get().contains(Flags::NEED_READ) {
                Status::Read
            } else {
                shared.tx_task.register(cx.waker());
                Status::Pause
            }
        } else {
            Status::Dropped
        }
    }
}

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
    struct Flags: u8 {
        const EOF         = 0b0000_0001;
        const ERROR       = 0b0000_0010;
        const NEED_READ   = 0b0000_0100;
        const SENDER_GONE = 0b0000_1000;
    }
}

struct Inner<E> {
    len: Cell<usize>,
    flags: Cell<Flags>,
    err: Cell<Option<E>>,
    items: RefCell<VecDeque<Bytes>>,
    max_size: Cell<usize>,
    rx_task: LocalWaker,
    tx_task: LocalWaker,
}

impl<E> Inner<E> {
    fn new(eof: bool) -> Self {
        let flags = if eof {
            Flags::EOF | Flags::NEED_READ
        } else {
            Flags::NEED_READ
        };
        Inner {
            flags: Cell::new(flags),
            len: Cell::new(0),
            err: Cell::new(None),
            items: RefCell::new(VecDeque::new()),
            rx_task: LocalWaker::new(),
            tx_task: LocalWaker::new(),
            max_size: Cell::new(MAX_BUFFER_SIZE),
        }
    }

    fn insert_flag(&self, f: Flags) {
        let mut flags = self.flags.get();
        flags.insert(f);
        self.flags.set(flags);
    }

    fn remove_flag(&self, f: Flags) {
        let mut flags = self.flags.get();
        flags.remove(f);
        self.flags.set(flags);
    }

    fn set_error(&self, err: E) {
        self.err.set(Some(err));
        self.insert_flag(Flags::ERROR);
        self.rx_task.wake()
    }

    fn feed_eof(&self) {
        self.insert_flag(Flags::EOF);
        self.rx_task.wake()
    }

    fn feed_data(&self, data: Bytes) {
        let len = self.len.get() + data.len();
        self.len.set(len);
        self.items.borrow_mut().push_back(data);
        if len < self.max_size.get() {
            self.insert_flag(Flags::NEED_READ);
        } else {
            self.remove_flag(Flags::NEED_READ);
        }
        self.rx_task.wake();
    }

    fn unread_data(&self, data: Bytes) {
        if !data.is_empty() {
            self.len.set(self.len.get() + data.len());
            self.items.borrow_mut().push_front(data);
        }
    }
}

impl<E> fmt::Debug for Inner<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Inner")
            .field("len", &self.len)
            .field("flags", &self.flags)
            .field("items", &self.items.borrow())
            .field("max_size", &self.max_size)
            .field("rx_task", &self.rx_task)
            .field("tx_task", &self.tx_task)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[ntex_macros::rt_test2]
    async fn test_eof() {
        let (_, rx) = eof::<()>();
        assert!(rx.read().await.is_none());
    }

    #[ntex_macros::rt_test2]
    async fn test_unread_data() {
        let (_, payload) = channel::<()>();

        payload.put(Bytes::from("data"));
        assert_eq!(payload.inner.len.get(), 4);
        assert_eq!(
            Bytes::from("data"),
            poll_fn(|cx| payload.poll_read(cx)).await.unwrap().unwrap()
        );
    }
}
