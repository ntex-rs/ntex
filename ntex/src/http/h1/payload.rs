//! Payload stream
use std::rc::{Rc, Weak};
use std::task::{Context, Poll};
use std::{cell::RefCell, collections::VecDeque, pin::Pin};

use bytes::Bytes;
use futures_core::Stream;

use crate::http::error::PayloadError;
use crate::task::LocalWaker;

/// max buffer size 32k
const MAX_BUFFER_SIZE: usize = 32_768;

#[derive(Debug, PartialEq)]
pub(super) enum PayloadStatus {
    Read,
    Pause,
    Dropped,
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
    pub fn create(eof: bool) -> (PayloadSender, Payload) {
        let shared = Rc::new(RefCell::new(Inner::new(eof)));

        (
            PayloadSender {
                inner: Rc::downgrade(&shared),
            },
            Payload { inner: shared },
        )
    }

    /// Create empty payload
    #[doc(hidden)]
    pub fn empty() -> Payload {
        Payload {
            inner: Rc::new(RefCell::new(Inner::new(true))),
        }
    }

    /// Put unused data back to payload
    #[inline]
    pub fn unread_data(&mut self, data: Bytes) {
        self.inner.borrow_mut().unread_data(data);
    }

    #[inline]
    pub fn readany(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, PayloadError>>> {
        self.inner.borrow_mut().readany(cx)
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

    pub fn feed_eof(&mut self) {
        if let Some(shared) = self.inner.upgrade() {
            shared.borrow_mut().feed_eof();
            self.inner = Weak::new();
        }
    }

    pub fn feed_data(&mut self, data: Bytes) {
        if let Some(shared) = self.inner.upgrade() {
            shared.borrow_mut().feed_data(data)
        }
    }

    pub(super) fn poll_data_required(&self, cx: &mut Context<'_>) -> PayloadStatus {
        // we check only if Payload (other side) is alive,
        // otherwise always return true (consume payload)
        if let Some(shared) = self.inner.upgrade() {
            if shared.borrow().need_read {
                PayloadStatus::Read
            } else {
                shared.borrow_mut().io_task.register(cx.waker());
                PayloadStatus::Pause
            }
        } else {
            PayloadStatus::Dropped
        }
    }
}

#[derive(Debug)]
struct Inner {
    len: usize,
    eof: bool,
    err: Option<PayloadError>,
    need_read: bool,
    items: VecDeque<Bytes>,
    task: LocalWaker,
    io_task: LocalWaker,
}

impl Inner {
    fn new(eof: bool) -> Self {
        Inner {
            eof,
            len: 0,
            err: None,
            items: VecDeque::new(),
            need_read: true,
            task: LocalWaker::new(),
            io_task: LocalWaker::new(),
        }
    }

    fn set_error(&mut self, err: PayloadError) {
        self.err = Some(err);
        self.task.wake()
    }

    fn feed_eof(&mut self) {
        self.eof = true;
        self.task.wake()
    }

    fn feed_data(&mut self, data: Bytes) {
        self.len += data.len();
        self.items.push_back(data);
        self.need_read = self.len < MAX_BUFFER_SIZE;
        self.task.wake();
    }

    fn readany(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, PayloadError>>> {
        if let Some(data) = self.items.pop_front() {
            self.len -= data.len();
            self.need_read = self.len < MAX_BUFFER_SIZE;

            if self.need_read && !self.eof {
                self.task.register(cx.waker());
            }
            self.io_task.wake();
            Poll::Ready(Some(Ok(data)))
        } else if let Some(err) = self.err.take() {
            Poll::Ready(Some(Err(err)))
        } else if self.eof {
            Poll::Ready(None)
        } else {
            self.need_read = true;
            self.task.register(cx.waker());
            self.io_task.wake();
            Poll::Pending
        }
    }

    fn unread_data(&mut self, data: Bytes) {
        self.len += data.len();
        self.items.push_front(data);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::poll_fn;

    #[crate::rt_test]
    async fn test_unread_data() {
        let (_, mut payload) = Payload::create(false);

        payload.unread_data(Bytes::from("data"));
        assert_eq!(payload.inner.borrow().len, 4);

        assert_eq!(
            Bytes::from("data"),
            poll_fn(|cx| payload.readany(cx)).await.unwrap().unwrap()
        );
    }
}
