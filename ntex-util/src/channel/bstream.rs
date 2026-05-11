//! Bytes stream
use std::cell::{Cell, RefCell};
use std::task::{Context, Poll};
use std::{collections::VecDeque, fmt, future::poll_fn, pin::Pin, rc::Rc, rc::Weak};

use ntex_bytes::Bytes;

use crate::{Stream, task::LocalWaker};

/// max buffer size 32k
const MAX_BUFFER_SIZE: usize = 32_768;

/// Indicates the current status of a byte stream.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// End of stream reached.
    Eof,
    /// Stream is ready to accept more bytes.
    Ready,
    /// The receiver side has been dropped.
    Dropped,
}

/// Creates a byte stream.
///
/// This method constructs two objects responsible for generating
/// and consuming the byte stream.
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

/// Creates an empty byte stream.
pub fn empty<E>(data: Option<Bytes>) -> Receiver<E> {
    let rx = Receiver {
        inner: Rc::new(Inner::new(true)),
    };
    if let Some(data) = data {
        rx.put(data);
    }
    rx
}

/// A buffered stream of byte chunks.
///
/// Incoming payload data is stored internally as a vector of chunks.
/// Chunks can be retrieved incrementally using the `.read()` method.
#[derive(Debug)]
pub struct Receiver<E> {
    inner: Rc<Inner<E>>,
}

impl<E> Receiver<E> {
    /// Sets the size of the stream buffer.
    ///
    /// By default, the buffer size is 32 KB.
    #[inline]
    pub fn max_buffer_size(&self, size: usize) {
        self.inner.max_buffer_size.set(size);
    }

    /// Puts unused data back into the stream.
    #[inline]
    pub fn put(&self, data: Bytes) {
        self.inner.unread_data(data);
    }

    #[inline]
    /// Returns `true` if the stream has reached EOF.
    pub fn is_eof(&self) -> bool {
        self.inner.flags.get().contains(Flags::EOF)
    }

    #[inline]
    /// Reads the next available chunk of bytes from the stream.
    pub async fn read(&self) -> Option<Result<Bytes, E>> {
        poll_fn(|cx| self.poll_read(cx)).await
    }

    #[inline]
    /// Attempts to read the next available chunk of bytes from the stream.
    pub fn poll_read(&self, cx: &mut Context<'_>) -> Poll<Option<Result<Bytes, E>>> {
        if let Some(data) = self.inner.get_data() {
            Poll::Ready(Some(Ok(data)))
        } else if let Some(err) = self.inner.err.take() {
            self.inner.insert_flag(Flags::EOF);
            Poll::Ready(Some(Err(err)))
        } else if self.inner.flags.get().intersects(Flags::EOF | Flags::ERROR) {
            Poll::Ready(None)
        } else {
            self.inner.recv_task.register(cx.waker());
            Poll::Pending
        }
    }
}

impl<E> Stream for Receiver<E> {
    type Item = Result<Bytes, E>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.poll_read(cx)
    }
}

impl<E> Drop for Receiver<E> {
    fn drop(&mut self) {
        self.inner.send_task.wake();
    }
}

/// Sender side of the byte stream.
///
/// It is possible to send data from a cloned sender, but readiness
/// checks apply only to the most recently used instance.
#[derive(Debug)]
pub struct Sender<E> {
    inner: Weak<Inner<E>>,
}

impl<E> Clone for Sender<E> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<E> Drop for Sender<E> {
    fn drop(&mut self) {
        if self.inner.weak_count() == 1
            && let Some(shared) = self.inner.upgrade()
        {
            shared.insert_flag(Flags::EOF | Flags::SENDER_GONE);
        }
    }
}

impl<E> Sender<E> {
    /// Returns whether this channel is closed.
    pub fn is_closed(&self) -> bool {
        self.inner.strong_count() == 0
    }

    /// Sets the stream error.
    pub fn set_error(&self, err: E) {
        if let Some(shared) = self.inner.upgrade() {
            shared.set_error(err);
        }
    }

    /// Marks the stream as EOF.
    pub fn feed_eof(&self) {
        if let Some(shared) = self.inner.upgrade() {
            shared.feed_eof();
        }
    }

    /// Adds a chunk to the stream.
    pub fn feed_data(&self, data: Bytes) {
        if let Some(shared) = self.inner.upgrade() {
            shared.feed_data(data);
        }
    }

    /// Checks whether the stream is ready for operation.
    pub async fn ready(&self) -> Status {
        poll_fn(|cx| self.poll_ready(cx)).await
    }

    /// Checks whether the stream is ready for operation.
    pub fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Status> {
        if let Some(shared) = self.inner.upgrade() {
            let flags = shared.flags.get();
            if flags.contains(Flags::NEED_READ) {
                Poll::Ready(Status::Ready)
            } else if flags.contains(Flags::SENDER_GONE | Flags::ERROR) {
                Poll::Ready(Status::Dropped)
            } else if flags.intersects(Flags::EOF) {
                Poll::Ready(Status::Eof)
            } else {
                shared.send_task.register(cx.waker());
                Poll::Pending
            }
        } else {
            // receiver is gone
            Poll::Ready(Status::Dropped)
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
    max_buffer_size: Cell<usize>,
    recv_task: LocalWaker,
    send_task: LocalWaker,
}

impl<E> Inner<E> {
    fn new(eof: bool) -> Self {
        let flags = if eof { Flags::EOF } else { Flags::NEED_READ };
        Inner {
            flags: Cell::new(flags),
            len: Cell::new(0),
            err: Cell::new(None),
            items: RefCell::new(VecDeque::new()),
            recv_task: LocalWaker::new(),
            send_task: LocalWaker::new(),
            max_buffer_size: Cell::new(MAX_BUFFER_SIZE),
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
        self.recv_task.wake();
        self.send_task.wake();
    }

    fn feed_eof(&self) {
        self.insert_flag(Flags::EOF);
        self.recv_task.wake();
        self.send_task.wake();
    }

    fn feed_data(&self, data: Bytes) {
        let len = self.len.get() + data.len();
        self.len.set(len);
        self.items.borrow_mut().push_back(data);
        self.recv_task.wake();

        if len >= self.max_buffer_size.get() {
            self.remove_flag(Flags::NEED_READ);
        }
    }

    fn get_data(&self) -> Option<Bytes> {
        self.items.borrow_mut().pop_front().inspect(|data| {
            let len = self.len.get() - data.len();

            // check size of stream buffer,
            // if stream has more space wake up sender
            self.len.set(len);
            if len < self.max_buffer_size.get() {
                self.insert_flag(Flags::NEED_READ);
                self.send_task.wake();
            }
        })
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
            .field("max_buffer_size", &self.max_buffer_size)
            .field("recv_task", &self.recv_task)
            .field("send_task", &self.send_task)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[ntex::test]
    async fn test_eof() {
        let (tx, rx) = eof::<()>();
        rx.max_buffer_size(100);
        assert!(rx.read().await.is_none());
        assert_eq!(tx.ready().await, Status::Eof);
    }

    #[ntex::test]
    async fn test_closed() {
        // drop receiver
        let (tx, rx) = channel::<()>();
        assert!(!tx.is_closed());
        drop(rx);
        assert!(tx.is_closed());

        // drop sender
        let (tx, rx) = channel::<()>();
        drop(tx);
        assert_eq!(rx.read().await, None);
    }

    #[ntex::test]
    async fn test_unread_data() {
        let (_, payload) = channel::<()>();

        payload.put(Bytes::from("data"));
        assert_eq!(payload.inner.len.get(), 4);
        assert_eq!(
            Bytes::from("data"),
            poll_fn(|cx| payload.poll_read(cx)).await.unwrap().unwrap()
        );
    }

    #[ntex::test]
    async fn test_sender_clone() {
        let (sender, payload) = channel::<()>();
        assert!(!payload.is_eof());
        let sender2 = sender.clone();
        assert!(!payload.is_eof());
        drop(sender2);
        assert!(!payload.is_eof());
        drop(sender);
        assert!(payload.is_eof());
    }
}
