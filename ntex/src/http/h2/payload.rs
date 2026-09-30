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
        const ERROR = 0b0000_0100;
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
        if let Some(f) = self.inner.on_drop.take() {
            f();
        }
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
        if let Some(shared) = self.inner.upgrade() {
            drop(shared.on_drop.take());
            shared.set_error(PayloadError::Incomplete(None));
        }
    }
}

impl PayloadSender {
    /// Checks if the payload stream is dropped.
    pub(crate) fn is_dropped(&self) -> bool {
        self.inner.strong_count() == 0
    }

    /// Closes the payload stream with an error.
    pub fn set_error(&self, err: PayloadError) {
        if let Some(shared) = self.inner.upgrade() {
            shared.set_error(err);
        }
    }

    /// Sends the final payload chunk and closes the stream.
    pub fn feed_eof(&self, data: Bytes, cap: Option<h2::Capacity>) {
        if let Some(shared) = self.inner.upgrade() {
            shared.feed_eof(data, cap);
        }
    }

    /// Sends a payload chunk and updates the HTTP/2 flow-control capacity.
    pub fn feed_data(&self, data: Bytes, cap: h2::Capacity) {
        if let Some(shared) = self.inner.upgrade() {
            shared.feed_data(data, cap);
        }
    }

    /// Registers a callback that runs if the payload is dropped while the sender is alive.
    pub(crate) fn on_drop(&self, f: impl FnOnce() + 'static) {
        if let Some(shared) = self.inner.upgrade() {
            shared.on_drop.set(Some(Box::new(f)));
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
    on_drop: Cell<Option<Box<dyn FnOnce()>>>,
}

impl Inner {
    fn new(cap: h2::Capacity) -> Self {
        Inner {
            cap: Cell::new(Some(cap)),
            flags: Cell::new(Flags::empty()),
            err: Cell::new(None),
            items: RefCell::new(VecDeque::new()),
            task: LocalWaker::new(),
            io_task: LocalWaker::new(),
            on_drop: Cell::new(None),
        }
    }

    fn insert_flags(&self, f: Flags) {
        let mut flags = self.flags.get();
        flags.insert(f);
        self.flags.set(flags);
    }

    fn set_error(&self, err: PayloadError) {
        // the first error is kept, a finished payload is not failed
        if !self.flags.get().intersects(Flags::EOF | Flags::ERROR) {
            self.insert_flags(Flags::ERROR);
            self.err.set(Some(err));
            self.task.wake();
        }
    }

    fn feed_eof(&self, data: Bytes, cap: Option<h2::Capacity>) {
        if let Some(cap) = cap {
            self.cap.set(Some(self.cap.take().unwrap() + cap));
        }
        self.insert_flags(Flags::EOF);
        if !data.is_empty() {
            self.items.borrow_mut().push_back(data);
        }
        self.task.wake();
    }

    fn feed_data(&self, data: Bytes, cap: h2::Capacity) {
        self.cap.set(Some(self.cap.take().unwrap() + cap));
        // empty DATA frames are not flow controlled, queueing them is unbounded
        if !data.is_empty() {
            self.items.borrow_mut().push_back(data);
            self.task.wake();
        }
    }

    fn readany(&self, cx: &mut Context<'_>) -> Poll<Option<Result<Bytes, PayloadError>>> {
        if let Some(data) = self.items.borrow_mut().pop_front() {
            let cap = self.cap.take().unwrap();
            cap.consume(data.len() as u32);
            self.cap.set(Some(cap));
            Poll::Ready(Some(Ok(data)))
        } else if let Some(err) = self.err.take() {
            // the payload ends after an error
            self.insert_flags(Flags::EOF);
            Poll::Ready(Some(Err(err)))
        } else if self.flags.get().contains(Flags::EOF) {
            Poll::Ready(None)
        } else {
            self.task.register(cx.waker());
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, atomic::AtomicUsize, atomic::Ordering};
    use std::task::Wake;

    use ntex_h2::client::SimpleClient;

    use super::*;
    use crate::http::{HeaderMap, Method, uri::Scheme};
    use crate::io::{Io, IoBoxed, testing::IoTest};
    use crate::{SharedCfg, time::Millis, time::sleep, util::ByteString};

    struct Counter(AtomicUsize);

    impl Wake for Counter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// A pending read does not wake the payload io task, only dropping the payload does.
    #[crate::rt_test]
    async fn test_pending_read_does_not_wake_io_task() {
        let (io, server) = IoTest::create();
        io.remote_buffer_cap(64 * 1024);
        let client = SimpleClient::new(
            IoBoxed::from(Io::new(io, SharedCfg::default())),
            Scheme::HTTP,
            ByteString::from_static("localhost"),
        );
        server.write([0, 0, 0, 4, 0, 0, 0, 0, 0]);
        sleep(Millis(50)).await;
        let (snd, _rcv) = client
            .send(Method::GET, "/".into(), HeaderMap::default(), true)
            .await
            .unwrap();

        let (sender, payload) = Payload::create(snd.stream().empty_capacity());
        let io_task = Arc::new(Counter(AtomicUsize::new(0)));
        let io_waker = io_task.clone().into();
        assert!(sender.on_cancel(&io_waker).is_pending());

        let reader = Arc::new(Counter(AtomicUsize::new(0)));
        let reader_waker = reader.clone().into();
        let mut cx = Context::from_waker(&reader_waker);
        for _ in 0..3 {
            assert!(payload.poll_read(&mut cx).is_pending());
        }
        assert_eq!(io_task.0.load(Ordering::SeqCst), 0);

        drop(payload);
        assert_eq!(io_task.0.load(Ordering::SeqCst), 1);
        assert!(sender.on_cancel(&io_waker).is_ready());
    }

    /// A ready read does not register the reader waker, new data does not wake
    /// a reader that is not waiting.
    #[crate::rt_test]
    async fn test_ready_read_does_not_register_waker() {
        let (io, server) = IoTest::create();
        io.remote_buffer_cap(64 * 1024);
        let client = SimpleClient::new(
            IoBoxed::from(Io::new(io, SharedCfg::default())),
            Scheme::HTTP,
            ByteString::from_static("localhost"),
        );
        server.write([0, 0, 0, 4, 0, 0, 0, 0, 0]);
        sleep(Millis(50)).await;
        let (snd, rcv) = client
            .send(Method::GET, "/".into(), HeaderMap::default(), true)
            .await
            .unwrap();

        // `200` response and two DATA frames
        server.write([0, 0, 1, 1, 4, 0, 0, 0, 1, 0x88]);
        server.write([0, 0, 2, 0, 0, 0, 0, 0, 1, b'a', b'b']);
        server.write([0, 0, 2, 0, 0, 0, 0, 0, 1, b'c', b'd']);
        let _ = rcv.recv().await.unwrap();
        let mut chunks = Vec::new();
        for _ in 0..2 {
            let msg = rcv.recv().await.unwrap();
            let h2::MessageKind::Data(data, cap) = msg.kind else {
                panic!("unexpected message: {msg:?}")
            };
            chunks.push((data, cap));
        }

        let (sender, payload) = Payload::create(snd.stream().empty_capacity());
        let reader = Arc::new(Counter(AtomicUsize::new(0)));
        let reader_waker = reader.clone().into();
        let mut cx = Context::from_waker(&reader_waker);

        let (data, cap) = chunks.remove(0);
        sender.feed_data(data, cap);
        assert!(matches!(payload.poll_read(&mut cx), Poll::Ready(Some(Ok(ref d))) if d == "ab"));

        let (data, cap) = chunks.remove(0);
        sender.feed_data(data, cap);
        assert_eq!(reader.0.load(Ordering::SeqCst), 0);
        assert!(matches!(payload.poll_read(&mut cx), Poll::Ready(Some(Ok(ref d))) if d == "cd"));
    }
}
