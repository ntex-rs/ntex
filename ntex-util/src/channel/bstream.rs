//! A buffered stream of byte chunks.
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

/// Creates a byte stream and returns its sender and receiver.
pub fn channel<E>() -> (Sender<E>, Receiver<E>) {
    let inner = Rc::new(Inner::new(false));

    (
        Sender {
            inner: Rc::downgrade(&inner),
        },
        Receiver { inner },
    )
}

/// Creates a byte stream that starts at EOF.
///
/// Data may still be added through the returned sender. The receiver yields
/// that buffered data first and then completes.
pub fn eof<E>() -> (Sender<E>, Receiver<E>) {
    let inner = Rc::new(Inner::new(true));

    (
        Sender {
            inner: Rc::downgrade(&inner),
        },
        Receiver { inner },
    )
}

/// Creates a receiver that contains optional data and is already at EOF.
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
/// The receiver yields chunks in insertion order. Its configured buffer size
/// is a cooperative backpressure threshold for the sender, not a hard memory
/// limit.
#[derive(Debug)]
pub struct Receiver<E> {
    inner: Rc<Inner<E>>,
}

impl<E> Receiver<E> {
    /// Sets the sender backpressure threshold.
    ///
    /// Once buffered data reaches this size, [`Sender::poll_ready`] stops
    /// reporting [`Status::Ready`] until the receiver consumes enough data.
    /// Sending does not enforce the threshold, so producers must cooperate by
    /// waiting for readiness. Changing the threshold immediately updates and,
    /// when needed, wakes sender readiness. The default is 32 KiB.
    #[inline]
    pub fn max_buffer_size(&self, size: usize) {
        self.inner.set_max_buffer_size(size);
    }

    /// Puts previously read data back at the front of the stream.
    ///
    /// This may grow the buffer past its readiness threshold.
    #[inline]
    pub fn put(&self, data: Bytes) {
        self.inner.unread_data(data);
    }

    #[inline]
    /// Returns `true` once EOF has been marked.
    ///
    /// Buffered chunks may still be available after this returns `true`.
    pub fn is_eof(&self) -> bool {
        self.inner.flags.get().contains(Flags::EOF)
    }

    #[inline]
    /// Waits for and returns the next chunk, stream error, or EOF.
    pub async fn read(&self) -> Option<Result<Bytes, E>> {
        poll_fn(|cx| self.poll_read(cx)).await
    }

    #[inline]
    /// Polls for the next chunk, stream error, or EOF.
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
/// Clones share one readiness registration. If several clones poll readiness,
/// the most recently registered task is the one that will be woken.
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
    /// Returns `true` if the receiver has been dropped.
    pub fn is_closed(&self) -> bool {
        self.inner.strong_count() == 0
    }

    /// Stores a terminal stream error and wakes both sides.
    pub fn set_error(&self, err: E) {
        if let Some(shared) = self.inner.upgrade() {
            shared.set_error(err);
        }
    }

    /// Marks the stream as EOF and wakes both sides.
    pub fn feed_eof(&self) {
        if let Some(shared) = self.inner.upgrade() {
            shared.feed_eof();
        }
    }

    /// Adds a chunk to the stream.
    ///
    /// This method does not enforce the configured backpressure threshold.
    pub fn feed_data(&self, data: Bytes) {
        if let Some(shared) = self.inner.upgrade() {
            shared.feed_data(data);
        }
    }

    /// Waits until the stream needs more data or reaches a terminal state.
    pub async fn ready(&self) -> Status {
        poll_fn(|cx| self.poll_ready(cx)).await
    }

    /// Polls until the stream needs more data or reaches a terminal state.
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

    fn set_max_buffer_size(&self, size: usize) {
        self.max_buffer_size.set(size);

        let flags = self.flags.get();
        if flags.intersects(Flags::EOF | Flags::ERROR | Flags::SENDER_GONE) {
            return;
        }

        if self.len.get() < size {
            if !flags.contains(Flags::NEED_READ) {
                self.insert_flag(Flags::NEED_READ);
                self.send_task.wake();
            }
        } else {
            self.remove_flag(Flags::NEED_READ);
        }
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
    use crate::future::lazy;

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
    async fn buffer_size_updates_sender_readiness() {
        let (tx, rx) = channel::<()>();
        rx.max_buffer_size(4);
        tx.feed_data(Bytes::from_static(b"data"));

        assert!(lazy(|cx| tx.poll_ready(cx)).await.is_pending());
        assert!(rx.inner.send_task.is_set());

        rx.max_buffer_size(5);
        assert!(!rx.inner.send_task.is_set());
        assert_eq!(
            lazy(|cx| tx.poll_ready(cx)).await,
            Poll::Ready(Status::Ready)
        );

        rx.max_buffer_size(4);
        assert!(lazy(|cx| tx.poll_ready(cx)).await.is_pending());
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
