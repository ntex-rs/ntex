//! A buffered stream of byte chunks.
use std::cell::{Cell, RefCell};
use std::task::{Context, Poll};
use std::{collections::VecDeque, fmt, future::poll_fn, pin::Pin, rc::Rc, rc::Weak};

use ntex_bytes::Bytes;
use ntex_service::ReadyNotifier;

use crate::{Stream, future::OnDropFn, task::LocalWaker};

/// Default high watermark, 32 KiB
const HIGH_WATERMARK: u32 = 32_768;
/// Default low watermark, 16 KiB
const LOW_WATERMARK: u32 = HIGH_WATERMARK / 2;

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
    channel_inner(Inner::new(false))
}

/// Creates a byte stream that invalidates cached sender readiness.
///
/// Subscribe to `changed` in [`ntex_service::Service::bind`] before using the
/// stream. The same notifier may be supplied to successive streams when a
/// service's active stream changes at runtime. Installing or removing that
/// active stream must also notify, since it changes the service's readiness.
///
/// Watermark and terminal-state transitions notify before waking waiters,
/// including when the receiver is dropped after a successful readiness check.
pub fn channel_with_notifier<E>(changed: ReadyNotifier) -> (Sender<E>, Receiver<E>) {
    channel_inner(Inner {
        changed: Some(changed),
        ..Inner::new(false)
    })
}

/// Creates a byte stream that starts at EOF.
///
/// Data may still be added through the returned sender. The receiver yields
/// that buffered data first and then completes.
pub fn eof<E>() -> (Sender<E>, Receiver<E>) {
    channel_inner(Inner::new(true))
}

fn channel_inner<E>(inner: Inner<E>) -> (Sender<E>, Receiver<E>) {
    let inner = Rc::new(inner);
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
    /// Sets the sender backpressure watermarks.
    ///
    /// Once buffered data reaches `high` bytes, [`Sender::poll_ready`] stops
    /// reporting [`Status::Ready`] until the receiver drains the buffer to
    /// `low` bytes or less, so the sender is not woken for every consumed
    /// chunk. `low` is capped below `high`.
    ///
    /// Sending does not enforce the watermarks, so producers must cooperate
    /// by waiting for readiness. Changing the watermarks immediately updates
    /// and, when needed, wakes sender readiness: the sender is ready if
    /// buffered data is below `high`. The defaults are 32 KiB and 16 KiB.
    #[inline]
    pub fn set_watermarks(&self, high: u32, low: u32) {
        self.inner.set_watermarks(high, low);
    }

    /// Sets the sender backpressure threshold.
    ///
    /// Sets the high watermark to `size` and the low watermark to half of it.
    #[inline]
    #[deprecated(since = "4.2.0", note = "Use `Receiver::set_watermarks()` instead")]
    pub fn max_buffer_size(&self, size: usize) {
        let size = u32::try_from(size).unwrap_or(u32::MAX);
        self.inner.set_watermarks(size, size / 2);
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
        let flags = self.inner.flags.get();
        self.inner.insert_flag(Flags::RECEIVER_GONE);
        self.inner.notify(flags, false, true);
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
            let flags = shared.flags.get();
            shared.insert_flag(Flags::EOF | Flags::SENDER_GONE);
            // a pending read completes with EOF
            shared.notify(flags, true, true);
        }
    }
}

impl<E> Sender<E> {
    /// Returns `true` if the receiver has been dropped.
    pub fn is_closed(&self) -> bool {
        self.inner
            .upgrade()
            .is_none_or(|shared| shared.flags.get().contains(Flags::RECEIVER_GONE))
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
    ///
    /// Terminal states take precedence: [`Status::Dropped`] is returned once
    /// the receiver is gone or an error was set, [`Status::Eof`] after EOF.
    pub fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Status> {
        if let Some(shared) = self.inner.upgrade() {
            let status = shared.flags.get().status();
            if status.is_pending() {
                shared.send_task.register(cx.waker());
            }
            status
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
        const RECEIVER_GONE = 0b0001_0000;
    }
}

impl Flags {
    fn status(self) -> Poll<Status> {
        if self.intersects(Self::SENDER_GONE | Self::RECEIVER_GONE | Self::ERROR) {
            Poll::Ready(Status::Dropped)
        } else if self.contains(Self::EOF) {
            Poll::Ready(Status::Eof)
        } else if self.contains(Self::NEED_READ) {
            Poll::Ready(Status::Ready)
        } else {
            Poll::Pending
        }
    }
}

struct Inner<E> {
    len: Cell<usize>,
    flags: Cell<Flags>,
    err: Cell<Option<E>>,
    items: RefCell<VecDeque<Bytes>>,
    recv_task: LocalWaker,
    send_task: LocalWaker,
    high_watermark: Cell<u32>,
    low_watermark: Cell<u32>,
    changed: Option<ReadyNotifier>,
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
            high_watermark: Cell::new(HIGH_WATERMARK),
            low_watermark: Cell::new(LOW_WATERMARK),
            changed: None,
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

    fn notify(&self, previous: Flags, recv: bool, send: bool) {
        let _wake = OnDropFn::new(|| {
            if recv {
                self.recv_task.wake();
            }
            if send {
                self.send_task.wake();
            }
        });
        if previous.status() != self.flags.get().status()
            && let Some(changed) = &self.changed
        {
            changed.notify();
        }
    }

    fn set_watermarks(&self, high: u32, low: u32) {
        self.high_watermark.set(high);
        self.low_watermark.set(low.min(high.saturating_sub(1)));

        let flags = self.flags.get();
        if flags.intersects(Flags::EOF | Flags::ERROR | Flags::SENDER_GONE) {
            return;
        }

        if self.len.get() < high as usize {
            if !flags.contains(Flags::NEED_READ) {
                self.insert_flag(Flags::NEED_READ);
                self.notify(flags, false, true);
            }
        } else {
            self.remove_flag(Flags::NEED_READ);
            self.notify(flags, false, false);
        }
    }

    fn set_error(&self, err: E) {
        let flags = self.flags.get();
        self.err.set(Some(err));
        self.insert_flag(Flags::ERROR);
        self.notify(flags, true, true);
    }

    fn feed_eof(&self) {
        let flags = self.flags.get();
        self.insert_flag(Flags::EOF);
        self.notify(flags, true, true);
    }

    fn feed_data(&self, data: Bytes) {
        let flags = self.flags.get();
        let len = self.len.get() + data.len();
        self.len.set(len);
        self.items.borrow_mut().push_back(data);
        if len >= self.high_watermark.get() as usize {
            self.remove_flag(Flags::NEED_READ);
        }
        self.notify(flags, true, false);
    }

    fn get_data(&self) -> Option<Bytes> {
        let data = self.items.borrow_mut().pop_front();
        data.inspect(|data| {
            let flags = self.flags.get();
            let len = self.len.get() - data.len();

            // wake up sender once the buffer is drained to the low watermark
            self.len.set(len);
            if len <= self.low_watermark.get() as usize {
                self.insert_flag(Flags::NEED_READ);
                self.notify(flags, false, true);
            }
        })
    }

    fn unread_data(&self, data: Bytes) {
        let flags = self.flags.get();
        if !data.is_empty() {
            self.len.set(self.len.get() + data.len());
            self.items.borrow_mut().push_front(data);
            if self.len.get() >= self.high_watermark.get() as usize {
                self.remove_flag(Flags::NEED_READ);
            }
            self.notify(flags, false, false);
        }
    }
}

impl<E> fmt::Debug for Inner<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Inner")
            .field("len", &self.len)
            .field("flags", &self.flags)
            .field("items", &self.items.borrow())
            .field("high_watermark", &self.high_watermark)
            .field("low_watermark", &self.low_watermark)
            .field("recv_task", &self.recv_task)
            .field("send_task", &self.send_task)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use ntex_service::{BindCtx, Ctx, Pipeline, ReadyCallback, Service};

    use super::*;
    use crate::future::lazy;

    struct SenderService(Sender<()>, ReadyNotifier);

    impl Service<(), Bytes> for SenderService {
        type Res = ();
        type Error = Status;

        fn bind(&self, ctx: BindCtx<'_>) {
            ctx.subscribe(&self.1);
        }

        async fn ready(&self, _: Ctx<'_, Self>) -> Result<(), Status> {
            match self.0.ready().await {
                Status::Ready => Ok(()),
                status => Err(status),
            }
        }

        async fn call(&self, data: Bytes, _: Ctx<'_, Self>) -> Result<(), Status> {
            self.0.feed_data(data);
            Ok(())
        }
    }

    #[ntex::test]
    async fn cached_sender_readiness_tracks_watermarks() {
        let changed = ReadyNotifier::new();
        let (tx, rx) = channel_with_notifier(changed.clone());
        rx.set_watermarks(4, 2);
        let srv = Pipeline::new((), SenderService(tx, changed));

        srv.call(Bytes::from_static(b"ab")).await.unwrap();
        assert!(srv.is_ready());
        srv.call(Bytes::from_static(b"cd")).await.unwrap();
        assert!(!srv.is_ready());
        assert!(lazy(|cx| srv.poll_ready(cx)).await.is_pending());
        assert_eq!(rx.read().await, Some(Ok(Bytes::from_static(b"ab"))));
        srv.ready().await.unwrap();

        rx.put(Bytes::from_static(b"ef"));
        assert!(!srv.is_ready());
        assert!(lazy(|cx| srv.poll_ready(cx)).await.is_pending());
        rx.set_watermarks(8, 4);
        srv.ready().await.unwrap();
        rx.set_watermarks(4, 2);
        assert!(!srv.is_ready());
        assert!(lazy(|cx| srv.poll_ready(cx)).await.is_pending());
    }

    #[ntex::test]
    async fn cached_sender_readiness_tracks_terminal_states() {
        for status in [Status::Eof, Status::Dropped] {
            let changed = ReadyNotifier::new();
            let (tx, _rx) = channel_with_notifier(changed.clone());
            let srv = Pipeline::new((), SenderService(tx.clone(), changed));
            srv.ready().await.unwrap();
            if status == Status::Eof {
                tx.feed_eof();
            } else {
                tx.set_error(());
            }
            assert!(!srv.is_ready());
            assert_eq!(srv.ready().await, Err(status));
        }

        let changed = ReadyNotifier::new();
        let (tx, rx) = channel_with_notifier(changed.clone());
        let srv = Pipeline::new((), SenderService(tx.clone(), changed));
        let on_drop = ReadyCallback::new(move || {
            assert!(tx.is_closed());
            assert_eq!(
                tx.poll_ready(&mut Context::from_waker(std::task::Waker::noop())),
                Poll::Ready(Status::Dropped)
            );
        });
        srv.on_notify(&on_drop);
        srv.ready().await.unwrap();
        drop(rx);
        assert!(!srv.is_ready());
        assert_eq!(srv.ready().await, Err(Status::Dropped));
    }

    #[ntex::test]
    async fn callback_panic_does_not_skip_stream_wakes() {
        use std::panic::{AssertUnwindSafe, catch_unwind};
        use std::sync::{Arc, atomic::AtomicUsize, atomic::Ordering};
        use std::task::{Wake, Waker};

        #[derive(Default)]
        struct WakeCount(AtomicUsize);
        impl Wake for WakeCount {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        let changed = ReadyNotifier::new();
        let (tx, rx) = channel_with_notifier(changed.clone());
        rx.set_watermarks(1, 0);
        let srv = Pipeline::new((), SenderService(tx.clone(), changed));
        srv.ready().await.unwrap();
        let armed = Rc::new(Cell::new(true));
        let armed2 = armed.clone();
        let callback = ReadyCallback::new(move || {
            assert!(!armed2.replace(false), "callback panic");
        });
        srv.on_notify(&callback);
        let wakes = Arc::new(WakeCount::default());
        let waker = Waker::from(wakes.clone());
        let mut cx = Context::from_waker(&waker);

        assert!(rx.poll_read(&mut cx).is_pending());
        assert!(catch_unwind(AssertUnwindSafe(|| tx.feed_data(b"x".into()))).is_err());
        assert!(!srv.is_ready());
        assert_eq!(wakes.0.load(Ordering::Relaxed), 1);
        assert!(tx.poll_ready(&mut cx).is_pending());

        armed.set(true);
        assert!(catch_unwind(AssertUnwindSafe(|| rx.poll_read(&mut cx))).is_err());
        assert_eq!(wakes.0.load(Ordering::Relaxed), 2);
        srv.ready().await.unwrap();
    }

    #[ntex::test]
    async fn test_sender_drop_wakes_receiver() {
        let (tx, rx) = channel::<()>();
        let handle = crate::spawn(async move { rx.read().await.is_none() });
        crate::time::sleep(crate::time::Millis(10)).await;

        drop(tx);
        let res = crate::time::timeout(crate::time::Millis(1000), handle).await;
        assert!(matches!(res, Ok(Ok(true))));
    }

    #[ntex::test]
    async fn test_eof() {
        let (tx, rx) = eof::<()>();
        rx.set_watermarks(100, 50);
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
        rx.set_watermarks(4, 2);
        tx.feed_data(Bytes::from_static(b"data"));

        assert!(lazy(|cx| tx.poll_ready(cx)).await.is_pending());
        assert!(rx.inner.send_task.is_set());

        rx.set_watermarks(5, 2);
        assert!(!rx.inner.send_task.is_set());
        assert_eq!(
            lazy(|cx| tx.poll_ready(cx)).await,
            Poll::Ready(Status::Ready)
        );

        rx.set_watermarks(4, 2);
        assert!(lazy(|cx| tx.poll_ready(cx)).await.is_pending());
    }

    #[ntex::test]
    async fn sender_resumes_at_low_watermark() {
        let (tx, rx) = channel::<()>();
        rx.set_watermarks(8, 4);
        for _ in 0..4 {
            tx.feed_data(Bytes::from_static(b"da"));
        }
        assert!(lazy(|cx| tx.poll_ready(cx)).await.is_pending());

        // above the low watermark
        assert!(rx.read().await.is_some());
        assert!(rx.inner.send_task.is_set());
        assert!(lazy(|cx| tx.poll_ready(cx)).await.is_pending());

        // at the low watermark
        assert!(rx.read().await.is_some());
        assert!(!rx.inner.send_task.is_set());
        assert_eq!(
            lazy(|cx| tx.poll_ready(cx)).await,
            Poll::Ready(Status::Ready)
        );

        // low watermark is capped below the high watermark
        let (tx, rx) = channel::<()>();
        rx.set_watermarks(4, 10);
        for _ in 0..3 {
            tx.feed_data(Bytes::from_static(b"da"));
        }
        assert!(rx.read().await.is_some());
        assert!(lazy(|cx| tx.poll_ready(cx)).await.is_pending());
        assert!(rx.read().await.is_some());
        assert_eq!(
            lazy(|cx| tx.poll_ready(cx)).await,
            Poll::Ready(Status::Ready)
        );
    }

    #[ntex::test]
    async fn custom_low_watermark() {
        let (tx, rx) = channel::<()>();
        rx.set_watermarks(8, 2);
        for _ in 0..4 {
            tx.feed_data(Bytes::from_static(b"da"));
        }
        assert!(rx.read().await.is_some());
        assert!(rx.read().await.is_some());
        assert!(lazy(|cx| tx.poll_ready(cx)).await.is_pending());
        assert!(rx.read().await.is_some());
        assert_eq!(
            lazy(|cx| tx.poll_ready(cx)).await,
            Poll::Ready(Status::Ready)
        );
    }

    #[ntex::test]
    #[allow(deprecated)]
    async fn deprecated_max_buffer_size() {
        let (_tx, rx) = channel::<()>();
        rx.max_buffer_size(10);
        assert_eq!(rx.inner.high_watermark.get(), 10);
        assert_eq!(rx.inner.low_watermark.get(), 5);
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

    #[ntex::test]
    async fn test_ready_terminal_states() {
        let (tx, _rx) = channel::<()>();
        assert_eq!(tx.ready().await, Status::Ready);
        tx.set_error(());
        assert_eq!(tx.ready().await, Status::Dropped);

        let (tx, _rx) = channel::<()>();
        tx.feed_eof();
        assert_eq!(tx.ready().await, Status::Eof);

        let (tx, rx) = channel::<()>();
        drop(rx);
        assert_eq!(tx.ready().await, Status::Dropped);
    }

    #[ntex::test]
    async fn test_empty() {
        let rx = empty::<()>(None);
        assert!(rx.is_eof());
        assert!(rx.read().await.is_none());

        let mut rx = empty::<()>(Some(Bytes::from_static(b"data")));
        assert!(format!("{rx:?}").contains("high_watermark"));
        assert_eq!(
            crate::future::stream_recv(&mut rx).await.unwrap().unwrap(),
            Bytes::from_static(b"data")
        );
        assert!(crate::future::stream_recv(&mut rx).await.is_none());
    }

    #[ntex::test]
    async fn test_error() {
        let (tx, rx) = channel::<&'static str>();
        tx.feed_data(Bytes::from_static(b"data"));
        tx.set_error("err");

        // buffered data is returned before the error, then eof
        assert_eq!(
            rx.read().await.unwrap().unwrap(),
            Bytes::from_static(b"data")
        );
        assert_eq!(rx.read().await.unwrap().unwrap_err(), "err");
        assert!(rx.is_eof());
        assert!(rx.read().await.is_none());
    }
}
