//! Payload stream
use std::collections::VecDeque;
use std::task::{Context, Poll, Waker};
use std::{cell::Cell, cell::RefCell, fmt, future::poll_fn, pin::Pin, rc::Rc, rc::Weak};

use ntex_h2::{self as h2};

use crate::util::{Bytes, Stream};
use crate::{http::error::PayloadError, task::LocalWaker};

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
    struct Flags: u8 {
        const EOF = 0b0000_0001;
        const DROPPED = 0b0000_0010;
    }
}

/// Buffered HTTP/2 payload stream.
///
/// Use [`Payload::read`] to receive the next chunk asynchronously, or consume
/// the payload through its [`Stream`] implementation. Waiting tasks are woken
/// when data, end-of-stream, or an error becomes available.
///
/// This type is not thread-safe and can also be used as a
/// [`Response`](crate::http::Response) body stream.
#[derive(Debug)]
pub struct Payload {
    inner: Rc<Inner>,
}

impl Payload {
    /// Create payload stream.
    ///
    /// This method construct two objects responsible for bytes stream
    /// generation.
    ///
    /// * `PayloadSender` - *Sender* side of the stream
    ///
    /// * `Payload` - *Receiver* side of the stream
    pub fn create(cap: h2::Capacity) -> (PayloadSender, Payload) {
        let shared = Rc::new(Inner::new(cap));

        (
            PayloadSender {
                inner: Rc::downgrade(&shared),
            },
            Payload { inner: shared },
        )
    }

    #[inline]
    /// Receives the next payload chunk.
    ///
    /// Returns `None` after the complete payload has been received.
    pub async fn read(&self) -> Option<Result<Bytes, PayloadError>> {
        poll_fn(|cx| self.poll_read(cx)).await
    }

    #[inline]
    /// Polls for the next payload chunk.
    pub fn poll_read(&self, cx: &mut Context<'_>) -> Poll<Option<Result<Bytes, PayloadError>>> {
        self.inner.readany(cx)
    }
}

impl Drop for Payload {
    fn drop(&mut self) {
        self.inner.io_task.wake();
        self.inner.insert_flags(Flags::DROPPED);
    }
}

impl Stream for Payload {
    type Item = Result<Bytes, PayloadError>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, PayloadError>>> {
        self.inner.readany(cx)
    }
}

#[derive(Debug)]
/// Sender part of the payload stream
pub struct PayloadSender {
    inner: Weak<Inner>,
}

impl Drop for PayloadSender {
    fn drop(&mut self) {
        if let Some(shared) = self.inner.upgrade()
            && !shared.flags.get().contains(Flags::EOF)
        {
            self.set_error(PayloadError::Incomplete(None));
        }
    }
}

impl PayloadSender {
    /// Closes the payload stream with an error.
    pub fn set_error(&self, err: PayloadError) {
        if let Some(shared) = self.inner.upgrade() {
            shared.set_error(err);
        }
    }

    /// Sends the final payload chunk and closes the stream.
    pub fn feed_eof(&self, data: Bytes) {
        if let Some(shared) = self.inner.upgrade() {
            shared.feed_eof(data);
        }
    }

    /// Sends a payload chunk and updates the HTTP/2 flow-control capacity.
    pub fn feed_data(&self, data: Bytes, cap: h2::Capacity) {
        if let Some(shared) = self.inner.upgrade() {
            shared.feed_data(data, cap);
        }
    }

    /// Associates the payload with its HTTP/2 stream.
    pub fn set_stream(&self, stream: Option<h2::Stream>) {
        if let Some(shared) = self.inner.upgrade() {
            shared.stream.set(stream);
        }
    }

    pub(crate) fn on_cancel(&self, w: &Waker) -> Poll<()> {
        if let Some(shared) = self.inner.upgrade() {
            if shared.flags.get().contains(Flags::DROPPED) {
                Poll::Ready(())
            } else {
                shared.io_task.register(w);
                Poll::Pending
            }
        } else {
            Poll::Ready(())
        }
    }
}

struct Inner {
    flags: Cell<Flags>,
    cap: Cell<Option<h2::Capacity>>,
    err: Cell<Option<PayloadError>>,
    items: RefCell<VecDeque<Bytes>>,
    task: LocalWaker,
    io_task: LocalWaker,
    stream: Cell<Option<h2::Stream>>,
}

impl Inner {
    fn new(cap: h2::Capacity) -> Self {
        Inner {
            cap: Cell::new(Some(cap)),
            flags: Cell::new(Flags::empty()),
            err: Cell::new(None),
            stream: Cell::new(None),
            items: RefCell::new(VecDeque::new()),
            task: LocalWaker::new(),
            io_task: LocalWaker::new(),
        }
    }

    fn insert_flags(&self, f: Flags) {
        let mut flags = self.flags.get();
        flags.insert(f);
        self.flags.set(flags);
    }

    fn set_error(&self, err: PayloadError) {
        self.err.set(Some(err));
        self.task.wake();
    }

    fn feed_eof(&self, data: Bytes) {
        self.insert_flags(Flags::EOF);
        if !data.is_empty() {
            self.items.borrow_mut().push_back(data);
        }
        self.task.wake();
    }

    fn feed_data(&self, data: Bytes, cap: h2::Capacity) {
        self.cap.set(Some(self.cap.take().unwrap() + cap));
        self.items.borrow_mut().push_back(data);
        self.task.wake();
    }

    fn readany(&self, cx: &mut Context<'_>) -> Poll<Option<Result<Bytes, PayloadError>>> {
        if let Some(data) = self.items.borrow_mut().pop_front() {
            if !self.flags.get().contains(Flags::EOF) {
                let cap = self.cap.take().unwrap();
                cap.consume(data.len() as u32);
                let size = cap.size();
                self.cap.set(Some(cap));

                if size == 0 {
                    self.task.register(cx.waker());
                }
            }
            Poll::Ready(Some(Ok(data)))
        } else if let Some(err) = self.err.take() {
            Poll::Ready(Some(Err(err)))
        } else if self.flags.get().contains(Flags::EOF) {
            Poll::Ready(None)
        } else {
            self.task.register(cx.waker());
            self.io_task.wake();
            Poll::Pending
        }
    }
}

impl fmt::Debug for Inner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let cap = self.cap.take().unwrap();
        let err = self.err.take();
        let result = f
            .debug_struct("Inner")
            .field("flags", &self.flags.get())
            .field("capacity", &cap)
            .field("error", &err)
            .field("items", &self.items.borrow())
            .finish();

        self.cap.set(Some(cap));
        self.err.set(err);
        result
    }
}
