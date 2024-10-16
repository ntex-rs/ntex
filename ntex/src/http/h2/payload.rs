//! Payload stream
use std::collections::VecDeque;
use std::task::{Context, Poll, Waker};
use std::{cell::RefCell, future::poll_fn, pin::Pin, rc::Rc, rc::Weak};

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

/// Buffered stream of byte chunks
///
/// Payload stores chunks in a vector. First chunk can be received with
/// `.readany()` method. Payload stream is not thread safe. Payload does not
/// notify current task when new data is available.
///
/// Payload stream can be used as `Response` body stream.
#[derive(Debug)]
pub struct Payload {
    inner: Rc<RefCell<Inner>>,
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
        let shared = Rc::new(RefCell::new(Inner::new(cap)));

        (
            PayloadSender {
                inner: Rc::downgrade(&shared),
            },
            Payload { inner: shared },
        )
    }

    #[inline]
    pub async fn read(&self) -> Option<Result<Bytes, PayloadError>> {
        poll_fn(|cx| self.poll_read(cx)).await
    }

    #[inline]
    pub fn poll_read(
        &self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, PayloadError>>> {
        self.inner.borrow_mut().readany(cx)
    }
}

impl Drop for Payload {
    fn drop(&mut self) {
        let mut inner = self.inner.borrow_mut();
        inner.io_task.wake();
        inner.flags.insert(Flags::DROPPED);
    }
}

impl Stream for Payload {
    type Item = Result<Bytes, PayloadError>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, PayloadError>>> {
        self.inner.borrow_mut().readany(cx)
    }
}

#[derive(Debug)]
/// Sender part of the payload stream
pub struct PayloadSender {
    inner: Weak<RefCell<Inner>>,
}

impl Drop for PayloadSender {
    fn drop(&mut self) {
        self.set_error(PayloadError::Incomplete(None))
    }
}

impl PayloadSender {
    pub fn set_error(&mut self, err: PayloadError) {
        if let Some(shared) = self.inner.upgrade() {
            shared.borrow_mut().set_error(err);
            self.inner = Weak::new();
        }
    }

    pub fn feed_eof(&mut self, data: Bytes) {
        if let Some(shared) = self.inner.upgrade() {
            shared.borrow_mut().feed_eof(data);
            self.inner = Weak::new();
        }
    }

    pub fn feed_data(&mut self, data: Bytes, cap: h2::Capacity) {
        if let Some(shared) = self.inner.upgrade() {
            shared.borrow_mut().feed_data(data, cap)
        }
    }

    pub fn set_stream(&self, stream: Option<h2::Stream>) {
        if let Some(shared) = self.inner.upgrade() {
            shared.borrow_mut().stream = stream;
        }
    }

    pub(crate) fn on_cancel(&self, w: &Waker) -> Poll<()> {
        if let Some(shared) = self.inner.upgrade() {
            if shared.borrow_mut().flags.contains(Flags::DROPPED) {
                Poll::Ready(())
            } else {
                shared.borrow_mut().io_task.register(w);
                Poll::Pending
            }
        } else {
            Poll::Ready(())
        }
    }
}

#[derive(Debug)]
struct Inner {
    flags: Flags,
    cap: h2::Capacity,
    err: Option<PayloadError>,
    items: VecDeque<Bytes>,
    task: LocalWaker,
    io_task: LocalWaker,
    stream: Option<h2::Stream>,
}

impl Inner {
    fn new(cap: h2::Capacity) -> Self {
        Inner {
            cap,
            flags: Flags::empty(),
            err: None,
            stream: None,
            items: VecDeque::new(),
            task: LocalWaker::new(),
            io_task: LocalWaker::new(),
        }
    }

    fn set_error(&mut self, err: PayloadError) {
        self.err = Some(err);
        self.task.wake()
    }

    fn feed_eof(&mut self, data: Bytes) {
        self.flags.insert(Flags::EOF);
        if !data.is_empty() {
            self.items.push_back(data);
        }
        self.task.wake()
    }

    fn feed_data(&mut self, data: Bytes, cap: h2::Capacity) {
        self.cap += cap;
        self.items.push_back(data);
        self.task.wake();
    }

    fn readany(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, PayloadError>>> {
        if let Some(data) = self.items.pop_front() {
            if !self.flags.contains(Flags::EOF) {
                self.cap.consume(data.len() as u32);

                if self.cap.size() == 0 {
                    self.task.register(cx.waker());
                }
            }
            Poll::Ready(Some(Ok(data)))
        } else if let Some(err) = self.err.take() {
            Poll::Ready(Some(Err(err)))
        } else if self.flags.contains(Flags::EOF) {
            Poll::Ready(None)
        } else {
            self.task.register(cx.waker());
            self.io_task.wake();
            Poll::Pending
        }
    }
}
