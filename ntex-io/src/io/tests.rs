use std::future::poll_fn;
use std::task::{Context, Poll, Waker};
use std::{cell::Cell, cell::RefCell, pin::Pin, rc::Rc, time::Duration};

use ntex_bytes::{BufMut, BytePages, Bytes, BytesMut};
use ntex_codec::{BytesCodec, Decoder};
use ntex_util::future::{Either, lazy};
use ntex_util::time::{Millis, sleep, timeout};

use super::*;
use crate::waiters::{TAG_DISCONNECT, TAG_WRITE, WaiterEntry};
use crate::{FilterBuf, IoContext, IoMetricsGroup, IoStatusUpdate, IoTaskStatus};
use crate::{Readiness, RecvError, Waiter, ops::Iops, testing::IoTest};

const BIN: &[u8] = b"GET /test HTTP/1\r\n\r\n";
const TEXT: &str = "GET /test HTTP/1\r\n\r\n";
const BIN2: &[u8] = b"12345678901234561234567890123456";

/// STATEMENT: Connections and their references compare equal to themselves.
///
/// HOW:
/// - Construct a test connection and check equality through both `Io` and `IoRef`.
#[ntex::test]
async fn test_basics() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);

    let server = Io::from(server);
    assert!(server.eq(&server));
    assert!(server.io_ref().eq(server.io_ref()));
}

/// STATEMENT: Receiving reports timeout notifications and still delivers input during write backpressure.
///
/// HOW:
/// - Inject a timeout and check its receive error, then write peer input and enable write backpressure.
/// - Verify the next receive yields the input bytes.
#[ntex::test]
async fn test_recv() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);

    let server = Io::new(server, SharedCfg::new("SRV"));

    server.st().notify_timeout();
    let err = server.recv(&BytesCodec).await.err().unwrap();
    assert!(format!("{err:?}").contains("Timeout"));

    client.write(TEXT);
    server.st().set_wr_backpressure();
    let item = server.recv(&BytesCodec).await.ok().unwrap().unwrap();
    assert_eq!(item, TEXT);
}

fn read_size_io(min: BytePageSize, max: BytePageSize) -> Io {
    Io::new(
        IoTest::create().1,
        SharedCfg::new("SRV").add(IoConfig::default().set_read_size(min, max)),
    )
}

/// STATEMENT: Read page size grows with accumulated read batches, bounded by the configured maximum.
///
/// HOW:
/// - Track partial, full, zero-byte, and oversized reads from the default minimum size.
/// - Check intermediate page sizes, allocated capacity, and saturation at the maximum.
#[ntex::test]
async fn read_size_grows_with_batch() {
    let io = Io::new(IoTest::create().1, SharedCfg::new("SRV"));
    let st = io.st();
    assert_eq!(st.read_size(), BytePageSize::Size4);
    assert_eq!(st.get_read_buf().capacity(), BytePageSize::Size4.capacity());

    // a read that does not fill its buffer ends the batch
    st.track_read(BytePageSize::Size4.capacity() - 1, false);
    assert_eq!(st.read_size(), BytePageSize::Size4);

    // full reads continue the batch, it grows the page once it no longer
    // fits
    st.track_read(3000, true);
    assert_eq!(st.read_size(), BytePageSize::Size4);
    st.track_read(3000, true);
    assert_eq!(st.read_size(), BytePageSize::Size8);
    st.track_read(10_000, false);
    assert_eq!(st.read_size(), BytePageSize::Size16);
    assert_eq!(
        st.get_read_buf().capacity(),
        BytePageSize::Size16.capacity()
    );

    // zero-byte reads and errors do not change anything
    st.track_read(0, false);
    assert_eq!(st.read_size(), BytePageSize::Size16);

    // capped at the max
    for _ in 0..10 {
        st.track_read(60_000, true);
    }
    assert_eq!(st.read_size(), BytePageSize::Size64);
    st.track_read(1, false);
    assert_eq!(st.read_size(), BytePageSize::Size64);
}

/// STATEMENT: Consecutive small batches shrink read pages one step at a time, never below the minimum.
///
/// HOW:
/// - Grow a page, track small batches around the shrink threshold, and interrupt them with a large batch.
/// - Verify the threshold restarts, then repeated small batches eventually reach the minimum size.
#[ntex::test]
async fn read_size_shrinks_after_small_batches() {
    let io = read_size_io(BytePageSize::Size8, BytePageSize::Size64);
    let st = io.st();
    assert_eq!(st.read_size(), BytePageSize::Size8);
    st.track_read(40_000, false);
    assert_eq!(st.read_size(), BytePageSize::Size48);

    // fits in half of the smaller page, but not enough in a row
    for _ in 0..RD_SHRINK_BATCHES - 1 {
        st.track_read(100, false);
    }
    assert_eq!(st.read_size(), BytePageSize::Size48);
    // a batch that needs the larger page restarts the count
    st.track_read(20_000, false);
    for _ in 0..RD_SHRINK_BATCHES - 1 {
        st.track_read(100, false);
    }
    assert_eq!(st.read_size(), BytePageSize::Size48);
    st.track_read(100, false);
    assert_eq!(st.read_size(), BytePageSize::Size32);

    // down to the min, not below
    for _ in 0..RD_SHRINK_BATCHES * 10 {
        st.track_read(100, false);
    }
    assert_eq!(st.read_size(), BytePageSize::Size8);
}

/// STATEMENT: Equal minimum and maximum read page sizes disable adaptation.
///
/// HOW:
/// - Configure a fixed page size and track repeated large and small batches.
/// - Verify the page size remains unchanged throughout.
#[ntex::test]
async fn read_size_fixed() {
    let io = read_size_io(BytePageSize::Size16, BytePageSize::Size16);
    let st = io.st();
    assert_eq!(st.read_size(), BytePageSize::Size16);
    for _ in 0..10 {
        st.track_read(60_000, true);
    }
    assert_eq!(st.read_size(), BytePageSize::Size16);
    for _ in 0..RD_SHRINK_BATCHES * 10 {
        st.track_read(1, false);
    }
    assert_eq!(st.read_size(), BytePageSize::Size16);
}

/// STATEMENT: Replacing configuration resets read adaptation to the new minimum and maximum.
///
/// HOW:
/// - Grow the default page size, replace the size range, and check reset to the new minimum.
/// - Track another large read and verify growth stops at the new maximum.
#[ntex::test]
async fn read_size_reset_by_set_config() {
    let io = Io::new(IoTest::create().1, SharedCfg::new("SRV"));
    io.st().track_read(60_000, false);
    assert_eq!(io.st().read_size(), BytePageSize::Size64);

    unsafe {
        io.set_config(
            SharedCfg::new("SRV2")
                .add(IoConfig::default().set_read_size(BytePageSize::Size8, BytePageSize::Size16)),
        );
    }
    assert_eq!(io.st().read_size(), BytePageSize::Size8);
    io.st().track_read(60_000, false);
    assert_eq!(io.st().read_size(), BytePageSize::Size16);
}

/// STATEMENT: Transport read sizes adapt to traffic independently of the read backpressure watermark.
///
/// HOW:
/// - Receive 100 KiB and verify page growth with an unchanged backpressure limit.
/// - Receive repeated one-byte messages and check the page size shrinks to the minimum.
#[ntex::test]
async fn read_size_follows_transport_reads() {
    let (client, server) = IoTest::create();
    let io = Io::new(server, SharedCfg::new("SRV"));
    assert_eq!(io.st().read_size(), BytePageSize::Size4);

    let data = vec![7u8; 100 * 1024];
    client.write(&data);
    let mut received = 0;
    while received < data.len() {
        let item = io.recv(&BytesCodec).await.unwrap().unwrap();
        received += item.len();
    }
    assert_eq!(received, data.len());
    assert!(io.st().read_size().capacity() > BytePageSize::Size4.capacity());
    assert_eq!(
        io.cfg().read_backpressure(),
        BytePageSize::Size32.capacity()
    );

    // small messages shrink it back
    for _ in 0..RD_SHRINK_BATCHES * 10 {
        client.write(b"x");
        assert_eq!(io.recv(&BytesCodec).await.unwrap().unwrap(), &b"x"[..]);
    }
    assert_eq!(io.st().read_size(), BytePageSize::Size4);
}
/// STATEMENT: Dropping `Io` releases callbacks that retain its connection reference.
///
/// HOW:
/// - Register a callback holding `IoRef`, retain a weak state reference, and drop the connection.
/// - Close the peer and verify the weak state reference can no longer be upgraded.
#[ntex::test]
async fn test_drop_releases_callbacks() {
    struct Cb(#[allow(dead_code)] IoRef);
    impl crate::IoCallbacks for Cb {
        fn before_processing(&self, _: &IoRef) {}
        fn after_processing(&self, _: &IoRef) {}
    }

    let (client, server) = IoTest::create();
    let server = Io::new(server, SharedCfg::new("SRV"));
    server.register_filter_callbacks(Cb(server.get_ref()));
    let state = Rc::downgrade(&server.io_ref().0);

    drop(server);
    client.close().await;
    sleep(Millis(50)).await;
    assert!(state.upgrade().is_none());
}

/// STATEMENT: Dropped or closed connections do not retain newly registered callbacks.
///
/// HOW:
/// - Register through a leftover reference during teardown and verify connection-state release.
/// - Register after shutdown, attempt callback dispatch, and check no callback runs.
#[ntex::test]
async fn test_callbacks_not_registered_after_drop_or_close() {
    struct Cb(#[allow(dead_code)] IoRef, Rc<Cell<usize>>);
    impl crate::IoCallbacks for Cb {
        fn before_processing(&self, _: &IoRef) {
            self.1.set(self.1.get() + 1);
        }
        fn after_processing(&self, _: &IoRef) {}
    }

    // dropped, teardown still in progress
    let (client, server) = IoTest::create();
    let server = Io::new(server, SharedCfg::new("SRV"));
    let io = server.get_ref();
    let state = Rc::downgrade(&io.0);
    drop(server);
    io.register_filter_callbacks(Cb(io.clone(), Rc::default()));
    drop(io);
    client.close().await;
    sleep(Millis(50)).await;
    assert!(state.upgrade().is_none());

    // closed, `Io` still alive
    let (client, server) = IoTest::create();
    let server = Io::new(server, SharedCfg::new("SRV"));
    client.close().await;
    server.close();
    let _ = server.shutdown().await;
    assert!(server.is_closed());
    let calls = Rc::new(Cell::new(0));
    server.register_filter_callbacks(Cb(server.get_ref(), calls.clone()));
    server.with_callbacks(|cb| cb.before_processing(&server));
    assert_eq!(calls.get(), 0);
}

/// STATEMENT: Filter callbacks can reenter waiter wake and registration operations.
///
/// HOW:
/// - Trigger a callback that wakes one tag and registers a waiter on another.
/// - Verify the first waiter completes and the new registration remains independently wakeable.
#[ntex::test]
async fn callbacks_reenter_waiters() {
    struct Cb(Rc<Waiter<'static>>);
    impl crate::IoCallbacks for Cb {
        fn before_processing(&self, io: &IoRef) {
            io.wake(7);
            let _ = self.0.poll_ready(&mut Context::from_waker(Waker::noop()));
        }
        fn after_processing(&self, _: &IoRef) {}
    }

    let (_client, server) = IoTest::create();
    let io = Io::new(server, SharedCfg::new("SRV"));
    let woken = io.waiter(7);
    assert!(lazy(|cx| woken.poll_ready(cx)).await.is_pending());
    let registered = Rc::new(io.waiter(8).into_static());
    io.register_filter_callbacks(Cb(registered.clone()));
    io.with_buf(|_| ()).unwrap();

    assert!(lazy(|cx| woken.poll_ready(cx)).await.is_ready());
    assert_eq!(io.get_ref().0.extensions.wakers_len(), 1);
    io.wake(8);
    assert!(lazy(|cx| registered.poll_ready(cx)).await.is_ready());
}

/// STATEMENT: A callback can drop its owning connection without retaining connection state.
///
/// HOW:
/// - Store `Io` in a callback-owned slot and trigger the callback to empty it.
/// - Release references, close the peer, and verify the weak state reference expires.
#[ntex::test]
async fn callbacks_dropping_io_release_state() {
    struct Cb(Rc<RefCell<Option<Io>>>, #[allow(dead_code)] IoRef);
    impl crate::IoCallbacks for Cb {
        fn before_processing(&self, _: &IoRef) {
            drop(self.0.borrow_mut().take());
        }
        fn after_processing(&self, _: &IoRef) {}
    }

    let (client, server) = IoTest::create();
    let server = Io::new(server, SharedCfg::new("SRV"));
    let io = server.get_ref();
    let state = Rc::downgrade(&io.0);
    let slot = Rc::new(RefCell::new(None));
    server.register_filter_callbacks(Cb(slot.clone(), io.clone()));
    *slot.borrow_mut() = Some(server);

    let _ = io.with_buf(|_| ());
    assert!(slot.borrow().is_none());
    drop(io);
    client.close().await;
    sleep(Millis(50)).await;
    assert!(state.upgrade().is_none());
}

thread_local! {
    static REENTER: RefCell<Option<(IoRef, usize)>> = const { RefCell::new(None) };
}

/// Wakes the tag of `REENTER` when woken or dropped.
struct Reenter;

impl Reenter {
    fn wake_tag() {
        if let Some((io, tag)) = REENTER.with_borrow(Clone::clone) {
            io.wake(tag);
        }
    }
}

impl std::task::Wake for Reenter {
    fn wake(self: Arc<Self>) {
        Self::wake_tag();
    }
}

impl Drop for Reenter {
    fn drop(&mut self) {
        Self::wake_tag();
    }
}

/// STATEMENT: Waiter wakers may reenter other tags from wake and drop callbacks.
///
/// HOW:
/// - Register a reentrant waker and trigger it by waking, replacing, and removing its waiter.
/// - Check the other tag's waiter completes after each action.
#[ntex::test]
async fn wakers_reenter_waiters() {
    let (_client, server) = IoTest::create();
    let io = Io::new(server, SharedCfg::new("SRV"));
    REENTER.set(Some((io.get_ref(), 8)));
    let (a, b) = (io.waiter(7), io.waiter(8));
    let reenter = || Waker::from(Arc::new(Reenter));
    let pending = async || assert!(lazy(|cx| b.poll_ready(cx)).await.is_pending());
    let woken = async || assert!(lazy(|cx| b.poll_ready(cx)).await.is_ready());

    // woken
    let waker = reenter();
    assert!(a.poll_ready(&mut Context::from_waker(&waker)).is_pending());
    pending().await;
    io.wake(7);
    woken().await;
    assert!(a.poll_ready(&mut Context::from_waker(&waker)).is_ready());
    drop(waker);

    // replaced
    assert!(
        a.poll_ready(&mut Context::from_waker(&reenter()))
            .is_pending()
    );
    pending().await;
    assert!(
        a.poll_ready(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    woken().await;

    // removed
    assert!(
        a.poll_ready(&mut Context::from_waker(&reenter()))
            .is_pending()
    );
    pending().await;
    drop(a);
    woken().await;

    REENTER.set(None);
}

/// STATEMENT: Stopping a timer clears its pending timeout notification.
///
/// HOW:
/// - Start a timer, inject a timeout, stop the timer, and verify status polling remains pending.
#[ntex::test]
async fn test_stop_timer_clears_timeout_notification() {
    let (_client, server) = IoTest::create();
    let server = Io::new(server, SharedCfg::new("SRV"));

    server.start_timer(ntex_util::time::Seconds(10));
    server.notify_timeout();
    server.stop_timer();

    assert!(lazy(|cx| server.poll_status_update(cx)).await.is_pending());
}

/// STATEMENT: Exact reads return requested bytes and report `UnexpectedEof` after disconnection.
///
/// HOW:
/// - Read four peer bytes, close the peer during another exact read, and check the error kind.
/// - Retry after EOF and verify the same error.
#[ntex::test]
async fn test_read() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);

    let server = Io::new(server, SharedCfg::new("SRV"));

    client.write(b"1234");
    let mut buf: [u8; 4] = [0, 0, 0, 0];
    server.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"1234");

    // disconnect during read
    let fut = ntex_rt::spawn(async move {
        let mut buf: [u8; 4] = [0, 0, 0, 0];
        let err = server.read_exact(&mut buf).await.unwrap_err();
        (server, err)
    });
    client.close().await;
    let (server, err) = fut.await.unwrap();
    assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);

    let err = server.read_exact(&mut [0]).await.unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
}

/// STATEMENT: An incomplete exact read at EOF preserves buffered bytes for a smaller read.
///
/// HOW:
/// - Send two bytes and close, request four bytes, and check `UnexpectedEof`.
/// - Read the original two bytes successfully, then verify another read reports EOF.
#[ntex::test]
async fn test_read_partial_eof() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);

    let server = Io::new(server, SharedCfg::new("SRV"));

    client.write(b"12");
    client.close().await;

    let err = server.read_exact(&mut [0; 4]).await.unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);

    let mut buf = [0; 2];
    server.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"12");

    let err = server.read_exact(&mut [0]).await.unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
}

/// STATEMENT: Sending a byte frame flushes the encoded payload to the peer.
///
/// HOW:
/// - Send a static payload through `BytesCodec` and compare the peer's received bytes.
#[ntex::test]
async fn test_send() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);

    let server = Io::from(server);
    assert!(server.eq(&server));

    server
        .send(Bytes::from_static(BIN), &BytesCodec)
        .await
        .ok()
        .unwrap();
    let item = client.read_any();
    assert_eq!(item, TEXT);
}

/// STATEMENT: Read backpressure pauses at the high watermark and resumes at the half-watermark.
///
/// HOW:
/// - Drive read completions manually, consume input around both thresholds, and inspect task wakes and flags.
/// - Verify byte order through repeated pressure cycles and termination readiness after force-close.
#[ntex::test]
async fn read() {
    let io = Io::new(
        IoTest::create().0,
        SharedCfg::new("SRV").add(
            IoConfig::default()
                .set_read_size(BytePageSize::Size4, BytePageSize::Size4)
                .set_read_backpressure(BytePageSize::Size4.capacity()),
        ),
    );
    assert!(lazy(|cx| io.poll_read_more(cx)).await.is_pending());
    assert!(io.st().dispatch_task.is_set());

    let high = BytePageSize::Size4.capacity();
    let half = high / 2;
    let data: Vec<u8> = (0..=u8::MAX).cycle().take(high + 2).collect();
    let ctx = IoContext::new(io.get_ref());

    // Ready
    assert_eq!(
        lazy(|cx| ctx.poll_read_ready(cx)).await,
        Poll::Ready(Readiness::Ready)
    );
    assert!(io.st().read_task.is_set());
    assert!(!io.st().flags.is_read_ready());
    assert!(!io.st().flags.is_rd_backpressure());
    assert!(!io.is_rd_backpressure());
    assert!(!io.is_wr_backpressure());

    // == Enable backpressure
    ctx.release_read_buf(
        BytesMut::copy_from_slice(&data),
        Poll::Ready(Ok(data.len())),
    );

    // dispatcher is woken
    assert!(!io.st().dispatch_task.is_set());
    // read task is paused
    assert!(io.st().flags.is_read_paused());
    // read buffer is ready
    assert!(io.st().flags.is_read_ready());
    // read backpressure is enabled
    assert!(io.st().flags.is_rd_backpressure());
    assert!(io.is_rd_backpressure());
    assert!(!io.is_wr_backpressure());
    // read task paused
    assert_eq!(lazy(|cx| ctx.poll_read_ready(cx)).await, Poll::Pending);

    // read one byte
    assert_eq!(io.with_read_dst(|buf| buf.split_to(1)), &data[..1]);
    // read buffer is ready
    assert!(io.st().flags.is_read_ready());
    // read backpressure is enabled
    assert!(io.st().flags.is_rd_backpressure());

    // read task is set
    assert!(io.st().read_task.is_set());

    // read one more byte
    assert_eq!(io.with_read_dst(|buf| buf.split_to(1)), &data[1..2]);
    // read backpressure is enabled
    assert!(io.st().flags.is_rd_backpressure());

    // dropping below the high watermark does not release backpressure
    assert_eq!(io.with_read_dst(|buf| buf.split_to(1)), &data[2..3]);
    assert!(io.st().flags.is_rd_backpressure());
    assert!(io.st().flags.is_read_paused());

    // reaching half of the high watermark releases backpressure
    let n = high - 1 - half;
    assert_eq!(io.with_read_dst(|buf| buf.split_to(n)), &data[3..3 + n]);
    // read task is not paused anymore
    assert!(!io.st().flags.is_read_paused());
    // read buffer is not ready
    assert!(!io.st().flags.is_read_ready());
    // read backpressure is disabled
    assert!(!io.st().flags.is_rd_backpressure());
    // read task is woken
    assert!(!io.st().read_task.is_set());
    assert_eq!(
        lazy(|cx| ctx.poll_read_ready(cx)).await,
        Poll::Ready(Readiness::Ready)
    );

    // register dispatcher task
    lazy(|cx| io.register_dispatch(cx)).await;

    // == Enable backpressure, half in buffer + half more
    ctx.release_read_buf(
        BytesMut::copy_from_slice(vec![b'a'; half]),
        Poll::Ready(Ok(half)),
    );

    // dispatcher is woken
    assert!(!io.st().dispatch_task.is_set());
    // read task is paused
    assert!(io.st().flags.is_read_paused());
    // read buffer is ready
    assert!(io.st().flags.is_read_ready());
    // read backpressure is enabled
    assert!(io.st().flags.is_rd_backpressure());
    // read task paused
    assert_eq!(lazy(|cx| ctx.poll_read_ready(cx)).await, Poll::Pending);

    // read half, buf size is half of high watermark
    assert_eq!(io.with_read_dst(|buf| buf.split_to(half)), &data[3 + n..]);
    // read backpressure is disabled
    assert!(!io.st().flags.is_rd_backpressure());

    // register dispatcher task
    lazy(|cx| io.register_dispatch(cx)).await;

    // == No backpressure, half in buffer + half - 1 more
    ctx.release_read_buf(
        BytesMut::copy_from_slice(vec![b'b'; half - 1]),
        Poll::Ready(Ok(half - 1)),
    );

    // read task is paused
    assert!(!io.st().flags.is_read_paused());
    // read buffer is ready
    assert!(io.st().flags.is_read_ready());
    // read backpressure is enabled
    assert!(!io.st().flags.is_rd_backpressure());
    // read task ready
    assert_eq!(
        lazy(|cx| ctx.poll_read_ready(cx)).await,
        Poll::Ready(Readiness::Ready)
    );

    // read everything
    let item = io.with_read_dst(BytesMut::take);
    assert_eq!(item.len(), high - 1);
    assert_eq!(&item[..half], &vec![b'a'; half][..]);
    assert_eq!(&item[half..], &vec![b'b'; half - 1][..]);
    // read task is paused
    assert!(!io.st().flags.is_read_paused());
    // read buffer is ready
    assert!(!io.st().flags.is_read_ready());
    // read task is not woken
    assert!(io.st().read_task.is_set());

    // == Terminate
    io.terminate();
    // read task is woken
    assert!(!io.st().read_task.is_set());
    // read task ready
    assert_eq!(
        lazy(|cx| ctx.poll_read_ready(cx)).await,
        Poll::Ready(Readiness::Terminate)
    );
}

/// STATEMENT: Only explicit force-close requests produce transport termination readiness.
///
/// HOW:
/// - Inject transport errors while active and shutting down and verify close readiness.
/// - Explicitly terminate another connection and verify read/write termination readiness.
#[ntex::test]
async fn only_force_close_reports_terminate() {
    // a transport failure ends the connection, but it is closed gracefully
    let io = Io::new(IoTest::create().0, SharedCfg::new("SRV"));
    let ctx = IoContext::new(io.get_ref());
    ctx.stop(Some(io::Error::other("transport failed")));
    assert!(io.st().flags.is_terminating());
    assert!(!io.st().flags.is_force_closing());
    assert_eq!(
        lazy(|cx| ctx.poll_read_ready(cx)).await,
        Poll::Ready(Readiness::Close)
    );
    assert_eq!(
        lazy(|cx| ctx.poll_write_ready(cx)).await,
        Poll::Ready(Readiness::Close)
    );

    // a failure during the transport shutdown phase does not abort either
    let io = Io::new(IoTest::create().0, SharedCfg::new("SRV"));
    let ctx = IoContext::new(io.get_ref());
    io.close();
    io.st().filters_stopped();
    assert!(io.st().flags.is_stopping());
    ctx.stop(Some(io::Error::other("transport failed")));
    assert_eq!(
        lazy(|cx| ctx.poll_write_ready(cx)).await,
        Poll::Ready(Readiness::Close)
    );

    // only an explicit force close aborts
    let io = Io::new(IoTest::create().0, SharedCfg::new("SRV"));
    let ctx = IoContext::new(io.get_ref());
    io.terminate();
    assert!(io.st().flags.is_force_closing());
    assert_eq!(
        lazy(|cx| ctx.poll_read_ready(cx)).await,
        Poll::Ready(Readiness::Terminate)
    );
    assert_eq!(
        lazy(|cx| ctx.poll_write_ready(cx)).await,
        Poll::Ready(Readiness::Terminate)
    );
}

/// STATEMENT: Dropping a connection with no outstanding output requests graceful transport closure.
///
/// HOW:
/// - Retain a context, verify no pending output, drop `Io`, and check non-forceful read/write close readiness.
#[ntex::test]
async fn drop_closes_gracefully_once_output_is_flushed() {
    // Nothing is left to deliver, so the transport ends the connection with
    // a normal FIN and the peer sees a clean end of stream.
    let io = Io::new(IoTest::create().0, SharedCfg::new("SRV"));
    let ioref = io.get_ref();
    let ctx = IoContext::new(io.get_ref());
    assert_eq!(io.st().write_outstanding(), 0);
    drop(io);
    assert!(!ioref.0.flags.is_force_closing());
    assert_eq!(
        lazy(|cx| ctx.poll_read_ready(cx)).await,
        Poll::Ready(Readiness::Close)
    );
    assert_eq!(
        lazy(|cx| ctx.poll_write_ready(cx)).await,
        Poll::Ready(Readiness::Close)
    );
}

/// STATEMENT: After `close()`, `encode` silently drops the item and reports `Ok`, while
/// `encode_slice` and `encode_bytes` report the closed connection.
///
/// HOW:
/// - Close an `Io` over an in-memory transport and verify `is_active` reports it.
/// - Call `encode` with `BytesCodec` and verify it returns `Ok` with no output buffered.
/// - Call `encode_slice` and `encode_bytes` and verify both return an `io::Error`.
#[ntex::test]
async fn encode_on_closed_io_drops_item_silently() {
    let (_client, server) = IoTest::create();
    let io = Io::new(server, SharedCfg::new("SRV"));
    io.close();
    assert!(!io.is_active());

    assert!(io.encode(Bytes::from_static(b"lost"), &BytesCodec).is_ok());
    assert_eq!(io.st().buffer.write_buf_size(), 0);
    assert!(io.encode_slice(b"lost").is_err());
    assert!(io.encode_bytes(Bytes::from_static(b"lost")).is_err());
    assert_eq!(io.st().buffer.write_buf_size(), 0);
}

/// STATEMENT: Dropping a connection with undeliverable output requests transport abort.
///
/// HOW:
/// - Queue output, drop `Io`, and verify force-close state and read/write termination readiness.
#[ntex::test]
async fn drop_aborts_when_output_would_be_lost() {
    // The filter chain goes away with the `Io`, so buffered output can
    // never be delivered. Aborting keeps a truncated stream distinguishable
    // from a complete one.
    let io = Io::new(IoTest::create().0, SharedCfg::new("SRV"));
    let ioref = io.get_ref();
    let ctx = IoContext::new(io.get_ref());
    io.encode_slice(b"not delivered").unwrap();
    assert_ne!(io.st().write_outstanding(), 0);
    drop(io);
    assert!(ioref.0.flags.is_force_closing());
    assert_eq!(
        lazy(|cx| ctx.poll_read_ready(cx)).await,
        Poll::Ready(Readiness::Terminate)
    );
    assert_eq!(
        lazy(|cx| ctx.poll_write_ready(cx)).await,
        Poll::Ready(Readiness::Terminate)
    );
}

/// STATEMENT: Dropping `Io` empties retained state buffers and discards late read completions.
///
/// HOW:
/// - Buffer input and output, retain the context and reference, then drop the connection.
/// - Check all buffers are empty before and after late reads through both completion APIs.
#[ntex::test]
async fn drop_releases_buffers() {
    // The state can outlive the `Io`, held by the transport while it closes
    // or by other handles, but nothing can use its buffers anymore.
    let io = Io::new(IoTest::create().0, SharedCfg::new("SRV"));
    let ioref = io.get_ref();
    let ctx = IoContext::new(io.get_ref());
    ctx.release_read_buf(BytesMut::copy_from_slice(b"unread"), Poll::Ready(Ok(6)));
    io.encode_slice(b"not delivered").unwrap();
    assert_eq!(ioref.0.buffer.read_dst_size(), 6);
    assert_ne!(ioref.0.buffer.write_buf_size(), 0);

    drop(io);
    assert_eq!(ioref.0.buffer.read_dst_size(), 0);
    assert!(ioref.0.buffer.get_read_buf().is_none());
    assert_eq!(ioref.0.buffer.write_buf_size(), 0);

    // input of a read that was in flight is discarded
    ctx.release_read_buf(BytesMut::copy_from_slice(b"late"), Poll::Ready(Ok(4)));
    assert!(ioref.0.buffer.get_read_buf().is_none());
    ctx.with_read_buf(|buf| {
        buf.extend_from_slice(b"late");
        Poll::Ready(Ok(4))
    });
    assert!(ioref.0.buffer.get_read_buf().is_none());
    assert_eq!(ioref.0.buffer.read_dst_size(), 0);
}

/// STATEMENT: Replacing the filter chain during drop preserves a prior force-close request.
///
/// HOW:
/// - Terminate and drop a connection, then verify its retained context still reports termination readiness.
#[ntex::test]
async fn force_close_survives_filter_replacement() {
    // Dropping `Io` swaps the chain for `NullFilter`, which cannot see the
    // io state, so an explicit terminate must still be honoured afterwards.
    let io = Io::new(IoTest::create().0, SharedCfg::new("SRV"));
    let ctx = IoContext::new(io.get_ref());
    io.terminate();
    drop(io);
    assert_eq!(
        lazy(|cx| ctx.poll_read_ready(cx)).await,
        Poll::Ready(Readiness::Terminate)
    );
    assert_eq!(
        lazy(|cx| ctx.poll_write_ready(cx)).await,
        Poll::Ready(Readiness::Terminate)
    );
}

/// STATEMENT: Read notifications wake the dispatcher and can rearm reads across backpressure cycles.
///
/// HOW:
/// - Register notifications, deliver input up to the watermark, and check readiness and wake flags.
/// - Rearm while pressured, deliver more input, and verify termination yields an end-of-input notification.
#[ntex::test]
async fn read_notify() {
    let io = Io::new(
        IoTest::create().0,
        SharedCfg::new("SRV").add(
            IoConfig::default()
                .set_read_size(BytePageSize::Size4, BytePageSize::Size4)
                .set_read_backpressure(BytePageSize::Size4.capacity()),
        ),
    );
    assert!(!io.st().flags.is_read_notify());
    assert!(lazy(|cx| io.poll_read_notify(cx)).await.is_pending());
    assert!(io.st().dispatch_task.is_set());
    assert!(io.st().flags.is_read_notify());

    let ctx = IoContext::new(io.get_ref());

    // incoming bytes
    ctx.release_read_buf(BytesMut::copy_from_slice(b"1"), Poll::Ready(Ok(1)));

    assert!(!io.st().dispatch_task.is_set());
    // rd buffer is ready
    assert!(io.st().flags.is_read_ready());
    assert!(io.st().flags.is_read_notify());
    // dispatcher is notified
    assert!(io.st().flags.is_read_notified());
    let res = lazy(|cx| io.poll_read_notify(cx)).await;
    assert!(matches!(res, Poll::Ready(Ok(Some(())))));

    // disapcher is not set
    assert!(!io.st().dispatch_task.is_set());
    // rd buffer is ready
    assert!(io.st().flags.is_read_ready());

    // == start notification process again
    assert!(lazy(|cx| io.poll_read_notify(cx)).await.is_pending());
    assert!(io.st().dispatch_task.is_set());
    assert!(io.st().flags.is_read_notify());
    assert!(io.st().flags.is_read_ready());
    // read task ready
    assert_eq!(
        lazy(|cx| ctx.poll_read_ready(cx)).await,
        Poll::Ready(Readiness::Ready)
    );

    // == enable packpressure
    let len = BytePageSize::Size4.capacity() - 1;
    ctx.release_read_buf(
        BytesMut::copy_from_slice(vec![b'2'; len]),
        Poll::Ready(Ok(len)),
    );
    // read backpressure is enabled
    assert!(io.st().flags.is_rd_backpressure());

    // rd buffer is ready
    assert!(io.st().flags.is_read_ready());
    assert!(io.st().flags.is_read_notify());
    // dispatcher is notified
    assert!(io.st().flags.is_read_notified());
    let res = lazy(|cx| io.poll_read_notify(cx)).await;
    assert!(matches!(res, Poll::Ready(Ok(Some(())))));
    // read task paused
    assert_eq!(lazy(|cx| ctx.poll_read_ready(cx)).await, Poll::Pending);
    // read task is set
    assert!(io.st().read_task.is_set());

    // == start notification process again
    assert!(lazy(|cx| io.poll_read_notify(cx)).await.is_pending());
    // read flags active
    assert!(!io.st().flags.is_rd_backpressure());
    assert!(!io.st().flags.is_read_ready());
    assert!(!io.st().flags.is_read_paused());
    // read task is woken
    assert!(!io.st().read_task.is_set());
    // read task ready
    assert_eq!(
        lazy(|cx| ctx.poll_read_ready(cx)).await,
        Poll::Ready(Readiness::Ready)
    );

    // incoming bytes
    ctx.release_read_buf(BytesMut::copy_from_slice(b"1"), Poll::Ready(Ok(1)));
    assert!(!io.st().dispatch_task.is_set());
    // rd buffer is ready
    assert!(io.st().flags.is_read_ready());
    assert!(io.st().flags.is_read_notify());
    assert!(io.st().flags.is_read_paused());
    assert!(io.st().flags.is_rd_backpressure());
    // dispatcher is notified
    assert!(io.st().flags.is_read_notified());
    assert!(matches!(
        lazy(|cx| io.poll_read_notify(cx)).await,
        Poll::Ready(Ok(Some(())))
    ));

    // == Terminate
    io.terminate();
    let res = lazy(|cx| io.poll_read_notify(cx)).await;
    assert!(matches!(res, Poll::Ready(Ok(None))), "{res:?}");
}

/// STATEMENT: Read-more polling remains ready while newly received input is available.
///
/// HOW:
/// - Poll an empty connection, send input, await readiness, and verify the buffered payload.
/// - Send another payload and check consecutive read-more polls are ready.
#[ntex::test]
async fn read_more() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);

    let io = Io::from(server);
    assert!(lazy(|cx| io.poll_read_more(cx)).await.is_pending());

    client.write(TEXT);
    assert_eq!(io.read_more().await.unwrap(), Some(()));
    assert!(matches!(
        lazy(|cx| io.poll_read_more(cx)).await,
        Poll::Ready(Ok(Some(())))
    ));

    let item = io.with_read_dst(BytesMut::take);
    assert_eq!(item, Bytes::from_static(BIN));

    client.write(TEXT);
    sleep(Millis(50)).await;
    assert!(lazy(|cx| io.poll_read_more(cx)).await.is_ready());
    assert!(lazy(|cx| io.poll_read_more(cx)).await.is_ready());
}

/// STATEMENT: Decoding buffered input releases read backpressure and later input can reenable it.
///
/// HOW:
/// - Fill the configured read watermark, receive the bytes, and check pressure/readiness flags clear.
/// - Fill it again and verify pressure and read-more availability.
#[ntex::test]
async fn read_backpressure() {
    let (client, server) = IoTest::create();

    let io = Io::new(
        server,
        SharedCfg::new("SRV").add(
            IoConfig::default()
                .set_read_size(BytePageSize::Size4, BytePageSize::Size4)
                .set_read_backpressure(BytePageSize::Size4.capacity()),
        ),
    );
    assert!(lazy(|cx| io.poll_read_more(cx)).await.is_pending());

    client.write(vec![b'x'; BytePageSize::Size4.capacity()]);
    sleep(Millis(50)).await;
    assert!(io.flags().is_read_ready());
    assert!(io.flags().is_rd_backpressure());
    let _item = io.recv(&BytesCodec).await.ok().unwrap().unwrap();
    assert!(!io.flags().is_read_ready());
    assert!(!io.flags().is_rd_backpressure());

    client.write(vec![b'x'; BytePageSize::Size4.capacity()]);
    sleep(Millis(50)).await;
    assert!(io.flags().is_read_ready());
    assert!(io.flags().is_rd_backpressure());
    assert_eq!(io.read_more().await.unwrap(), Some(()));
}

/// STATEMENT: Read backpressure metrics track periods, buffered bytes, and cleanup on consumption or drop.
///
/// HOW:
/// - Fill, hold, and consume input while checking pressure counts, gauges, and elapsed pressure time.
/// - Fill again, drop the connection, and verify gauges clear and pressure time stops advancing.
#[ntex::test]
async fn read_backpressure_metrics() {
    let cfg = SharedCfg::new("SRV")
        .add(
            IoConfig::default()
                .set_metrics(true)
                .set_read_size(BytePageSize::Size4, BytePageSize::Size4)
                .set_read_backpressure(BytePageSize::Size4.capacity()),
        )
        .build();
    let metrics = cfg.get::<IoConfig>().metrics();

    let (client, server) = IoTest::create();
    let io = Io::new(server, cfg);
    assert!(lazy(|cx| io.poll_read_more(cx)).await.is_pending());

    let size = BytePageSize::Size4.capacity();
    client.write(vec![b'x'; size]);
    sleep(Millis(50)).await;
    assert!(io.flags().is_rd_backpressure());
    let m = metrics.snapshot();
    assert_eq!(m.read_backpressure, 1);
    assert_eq!(m.read_backpressure_active, 1);
    assert_eq!(m.read_buffered, size);
    assert_eq!(m.write_backpressure, 0);

    sleep(Millis(50)).await;
    let _item = io.recv(&BytesCodec).await.ok().unwrap().unwrap();
    assert!(!io.flags().is_rd_backpressure());
    let m = metrics.snapshot();
    assert_eq!(m.read_backpressure, 1);
    assert_eq!(m.read_backpressure_active, 0);
    assert_eq!(m.read_buffered, 0);
    assert!(m.read_backpressure_time >= Duration::from_millis(40));
    let time = m.read_backpressure_time;

    // drop under backpressure closes the period and clears gauges
    client.write(vec![b'x'; size]);
    sleep(Millis(50)).await;
    assert!(io.flags().is_rd_backpressure());
    let m = metrics.snapshot();
    assert_eq!(m.read_backpressure, 2);
    assert_eq!(m.read_backpressure_active, 1);
    assert_eq!(m.read_buffered, size);

    drop(io);
    sleep(Millis(10)).await;
    let m = metrics.snapshot();
    assert_eq!(m.read_backpressure, 2);
    assert_eq!(m.read_backpressure_active, 0);
    assert_eq!(m.read_buffered, 0);
    assert!(m.read_backpressure_time > time);
    assert_eq!(
        metrics.snapshot().read_backpressure_time,
        m.read_backpressure_time
    );
}

/// STATEMENT: Read-buffer gauges track held pages, move with configuration, and clear after consumption or drop.
///
/// HOW:
/// - Accumulate partial input, verify one page's capacity and byte count, then consume it.
/// - Buffer more input, switch metric configurations, and check gauge transfer and final cleanup.
#[ntex::test]
async fn read_buffer_capacity_metrics() {
    let cfg = |tag| {
        SharedCfg::new(tag)
            .add(
                IoConfig::default()
                    .set_metrics(true)
                    .set_read_size(BytePageSize::Size4, BytePageSize::Size4),
            )
            .build()
    };
    let (cfg, cfg2) = (cfg("A"), cfg("B"));
    let (m1, m2) = (
        cfg.get::<IoConfig>().metrics(),
        cfg2.get::<IoConfig>().metrics(),
    );
    let held = |m: &IoMetrics| {
        let s = m.snapshot();
        (s.read_buffers_held, s.read_buffer_capacity, s.read_buffered)
    };
    let page = BytePageSize::Size4.capacity();

    let (client, server) = IoTest::create();
    let io = Io::new(server, cfg);
    assert!(lazy(|cx| io.poll_read_more(cx)).await.is_pending());
    assert_eq!(held(&m1), (0, 0, 0));

    // unconsumed input keeps a whole page
    client.write(b"partial");
    sleep(Millis(50)).await;
    assert_eq!(held(&m1), (1, page, 7));
    client.write(b"more");
    sleep(Millis(50)).await;
    assert_eq!(held(&m1), (1, page, 11));

    // consumed input releases the buffer
    assert_eq!(
        io.recv(&BytesCodec).await.unwrap().unwrap(),
        Bytes::from_static(b"partialmore")
    );
    assert_eq!(held(&m1), (0, 0, 0));

    // the gauges move with the configuration and clear on drop
    client.write(b"partial");
    sleep(Millis(50)).await;
    // SAFETY: no reference returned by `io.cfg()` is retained.
    unsafe { io.set_config(cfg2) };
    assert_eq!(held(&m1), (0, 0, 0));
    assert_eq!(held(&m2), (1, page, 7));
    drop(io);
    assert_eq!(held(&m2), (0, 0, 0));
}

/// STATEMENT: Write pressure is counted per period, with buffered bytes and active gauges cleared on drain or drop.
///
/// HOW:
/// - Block peer writes, queue output across the watermark, and verify repeated writes do not recount pressure.
/// - Unblock and flush, then enter pressure again and drop the connection while checking gauges and duration.
#[ntex::test]
async fn write_backpressure_metrics() {
    let cfg = SharedCfg::new("SRV")
        .add(
            IoConfig::default()
                .set_metrics(true)
                .set_write_backpressure(16),
        )
        .build();
    let metrics = cfg.get::<IoConfig>().metrics();

    let (client, server) = IoTest::create();
    client.remote_buffer_cap(0);
    let io = Io::new(server, cfg);
    assert!(lazy(|cx| io.poll_read_more(cx)).await.is_pending());

    io.encode_slice(b"1234").unwrap();
    let m = metrics.snapshot();
    assert_eq!(m.write_backpressure, 0);
    assert_eq!(m.write_buffered, 4);

    io.encode_slice(BIN2).unwrap();
    assert!(io.flags().is_wr_backpressure());
    let m = metrics.snapshot();
    assert_eq!(m.write_backpressure, 1);
    assert_eq!(m.write_backpressure_active, 1);
    assert_eq!(m.write_buffered, 4 + BIN2.len());

    // already enabled, not counted again
    io.encode_slice(b"1234").unwrap();
    let m = metrics.snapshot();
    assert_eq!(m.write_backpressure, 1);
    assert_eq!(m.write_buffered, 8 + BIN2.len());

    sleep(Millis(50)).await;
    client.remote_buffer_cap(1024);
    let _ = client.read().await.unwrap();
    assert!(matches!(
        lazy(|cx| io.poll_flush(cx, false)).await,
        Poll::Ready(Ok(()))
    ));
    assert!(!io.flags().is_wr_backpressure());
    let m = metrics.snapshot();
    assert_eq!(m.write_backpressure, 1);
    assert_eq!(m.write_backpressure_active, 0);
    assert_eq!(m.write_buffered, 0);
    assert!(m.write_backpressure_time >= Duration::from_millis(40));

    // drop under backpressure
    client.remote_buffer_cap(0);
    io.encode_slice(BIN2).unwrap();
    io.encode_slice(BIN2).unwrap();
    assert!(io.flags().is_wr_backpressure());
    let m = metrics.snapshot();
    assert_eq!(m.write_backpressure, 2);
    assert_eq!(m.write_backpressure_active, 1);
    assert!(m.write_buffered > 0);

    drop(io);
    sleep(Millis(10)).await;
    let m = metrics.snapshot();
    assert_eq!(m.write_backpressure_active, 0);
    assert_eq!(m.write_buffered, 0);
}

/// STATEMENT: I/O metrics distinguish complete and partial eager writes, scheduled batches, and transferred bytes.
///
/// HOW:
/// - Use budgeted transports above and below the eager threshold, including disabled eager writes.
/// - Check eager/partial/scheduled counters and written bytes, then receive input and check read bytes.
#[ntex::test]
async fn write_metrics() {
    #[derive(Debug)]
    struct Budget(Rc<Cell<usize>>);

    impl IoStream for Budget {
        fn start(self, _: IoContext) -> Box<dyn Handle> {
            Box::new(self)
        }
    }

    impl Handle for Budget {
        fn write(&self, ctx: &IoContext) {
            let n = ctx.with_write_dst(|buf| {
                let n = buf.len().min(self.0.get());
                buf.advance_to(n);
                n
            });
            self.0.set(self.0.get() - n);
            let _ = ctx.update_write_status(Ok(n));
        }
    }

    let cfg = SharedCfg::new("SRV")
        .add(IoConfig::new().set_metrics(true).set_write_buf_threshold(4))
        .build();
    let cfg0 = SharedCfg::new("SRV")
        .add(IoConfig::new().set_metrics(true).set_write_buf_threshold(0))
        .build();
    let metrics = cfg.get::<IoConfig>().metrics();
    let new_io =
        |budget: usize, cfg: &SharedCfg| Io::new(Budget(Rc::new(Cell::new(budget))), cfg.clone());

    // threshold reached, everything is written in place
    let io = new_io(usize::MAX, &cfg);
    io.encode_slice(&[0; 8]).unwrap();
    let m = metrics.snapshot();
    assert_eq!((m.eager_writes, m.eager_writes_partial), (1, 0));
    assert_eq!((m.scheduled_writes, m.bytes_written), (0, 8));

    // threshold reached, the transport takes only a part
    let io2 = new_io(3, &cfg);
    io2.encode_slice(&[0; 8]).unwrap();
    let m = metrics.snapshot();
    assert_eq!((m.eager_writes, m.eager_writes_partial), (2, 1));
    assert_eq!((m.scheduled_writes, m.bytes_written), (0, 11));

    // below threshold, the write task is scheduled once
    let io3 = new_io(usize::MAX, &cfg);
    io3.encode_slice(&[0; 2]).unwrap();
    io3.encode_slice(&[0; 1]).unwrap();
    let m = metrics.snapshot();
    assert_eq!((m.eager_writes, m.scheduled_writes), (2, 1));

    // eager writes are disabled
    let io4 = new_io(usize::MAX, &cfg0);
    io4.encode_slice(&[0; 8]).unwrap();
    let m = cfg0.get::<IoConfig>().metrics().snapshot();
    assert_eq!((m.eager_writes, m.scheduled_writes), (0, 1));
    assert_eq!(metrics.snapshot().scheduled_writes, 1);
    drop((io, io2, io3, io4));

    // bytes read
    let (client, server) = IoTest::create();
    let io = Io::new(server, cfg);
    client.write(BIN);
    assert_eq!(io.recv(&BytesCodec).await.unwrap().unwrap(), BIN);
    assert_eq!(metrics.snapshot().bytes_read, BIN.len() as u64);
}

/// STATEMENT: Close metrics retain the first reason and count each real connection once.
///
/// HOW:
/// - Close connections with competing reasons, taken placeholders, custom reasons, and transport failure.
/// - Verify total and per-reason counts, including no duplicate count for a taken placeholder.
#[ntex::test]
async fn close_reason_metrics() {
    IoMetricsGroup::set_current(IoMetricsGroup::new());
    let metrics = SharedCfg::default().get::<IoConfig>().metrics();

    // the first reported reason is kept
    let (_client, server) = IoTest::create();
    let io = Io::from(server);
    assert_eq!(io.close_reason(), None);
    io.set_close_reason(CloseReason::KeepAlive);
    io.set_close_reason(CloseReason::Decoder);
    assert_eq!(io.close_reason(), Some(CloseReason::KeepAlive));
    // a taken io leaves a placeholder that is not counted
    let io2 = unsafe { io.take() };
    drop(io);
    drop(io2);

    // closed without a reason
    let (_client, server) = IoTest::create();
    drop(Io::from(server));

    // application-defined reasons
    for reason in [
        CloseReason::Custom1,
        CloseReason::Custom8,
        CloseReason::Custom8,
    ] {
        let (_client, server) = IoTest::create();
        let io = Io::from(server);
        io.set_close_reason(reason);
        drop(io);
    }

    // transport error without a reason
    let (client, server) = IoTest::create();
    let io = Io::from(server);
    client.read_error(io::Error::other("err"));
    assert!(io.recv(&BytesCodec).await.is_err());
    drop(io);

    sleep(Millis(10)).await;
    let c = metrics.snapshot().closed;
    assert_eq!(c.total, 6);
    assert_eq!(c.custom, [1, 0, 0, 0, 0, 0, 0, 2]);
    assert_eq!(c.keepalive, 1);
    assert_eq!(c.decoder, 0);
    assert_eq!(c.other, 1);
    assert_eq!(c.io, 1);
}

/// STATEMENT: Metrics groups isolate dedicated configurations and refresh cached fallback mappings.
///
/// HOW:
/// - Compare disabled, enabled, and registered configurations and inspect group snapshot identities.
/// - Replace the current group, then register a configuration from another thread and verify remapping.
#[ntex::test]
async fn metrics_group() {
    let group = IoMetricsGroup::new();
    IoMetricsGroup::set_current(group.clone());
    assert!(Arc::ptr_eq(&IoMetricsGroup::current(), &group));
    assert_eq!(group.snapshot(), []);

    // configurations without metrics share the group's `other` metrics
    let srv = SharedCfg::new("SRV").build();
    let cli = SharedCfg::new("CLI").build();
    let other = srv.get::<IoConfig>().metrics();
    assert!(Arc::ptr_eq(&other, group.other()));
    assert!(Arc::ptr_eq(&other, &cli.get::<IoConfig>().metrics()));
    let _io = Io::new(IoTest::create().1, cli.clone());
    assert_eq!(group.snapshot(), []);

    // enabled and registered configurations have their own metrics
    let app = SharedCfg::new("APP")
        .add(IoConfig::new().set_metrics(true))
        .build();
    let metrics = app.get::<IoConfig>().metrics();
    assert!(!Arc::ptr_eq(&metrics, &other));
    assert!(Arc::ptr_eq(&metrics, &app.get::<IoConfig>().metrics()));
    group.register(&srv.get::<IoConfig>());
    let srv_metrics = srv.get::<IoConfig>().metrics();
    assert!(!Arc::ptr_eq(&srv_metrics, &other));
    assert!(!Arc::ptr_eq(&srv_metrics, &metrics));

    let _io2 = Io::new(IoTest::create().1, srv.clone());
    let _io3 = Io::new(IoTest::create().1, srv.clone());
    let mut items = group.snapshot();
    items.sort_unstable_by_key(|m| m.tag);
    assert_eq!(items.len(), 2);
    assert_eq!((items[0].id, items[0].tag), (app.id(), "APP"));
    assert_eq!((items[1].id, items[1].tag), (srv.id(), "SRV"));

    // another group has its own metrics
    IoMetricsGroup::set_current(IoMetricsGroup::new());
    assert!(!Arc::ptr_eq(&metrics, &app.get::<IoConfig>().metrics()));
    assert!(Arc::ptr_eq(
        &IoMetricsGroup::current().other().clone(),
        &srv.get::<IoConfig>().metrics()
    ));

    // registration on another thread replaces a cached `other` mapping
    let group = IoMetricsGroup::current();
    assert!(Arc::ptr_eq(&cli.get::<IoConfig>().metrics(), group.other()));
    let (g, cfg) = (group.clone(), cli.clone());
    std::thread::spawn(move || g.register(&cfg.get::<IoConfig>()))
        .join()
        .unwrap();
    let cli_metrics = cli.get::<IoConfig>().metrics();
    assert!(!Arc::ptr_eq(&cli_metrics, group.other()));
    assert!(Arc::ptr_eq(
        &cli_metrics,
        &group.get(&cli.get::<IoConfig>())
    ));
}

/// STATEMENT: Configuration changes transfer active write gauges without transferring historical events.
///
/// HOW:
/// - Enter write backpressure, replace the configuration, and compare old/new pressure and byte gauges.
/// - Drop the connection and verify cleanup and closure are attributed only to the new configuration.
#[ntex::test]
async fn set_config_moves_metrics() {
    let cfg = SharedCfg::new("A")
        .add(
            IoConfig::default()
                .set_metrics(true)
                .set_write_backpressure(16),
        )
        .build();
    let cfg2 = SharedCfg::new("B")
        .add(IoConfig::default().set_metrics(true))
        .build();
    let (m1, m2) = (
        cfg.get::<IoConfig>().metrics(),
        cfg2.get::<IoConfig>().metrics(),
    );

    let (client, server) = IoTest::create();
    client.remote_buffer_cap(0);
    let io = Io::new(server, cfg);
    io.encode_slice(&[b'x'; 32]).unwrap();
    sleep(Millis(10)).await;
    assert!(io.flags().is_wr_backpressure());
    assert_eq!(m1.snapshot().write_backpressure_active, 1);

    // SAFETY: no reference returned by `io.cfg()` is retained.
    unsafe { io.set_config(cfg2) };
    let (s1, s2) = (m1.snapshot(), m2.snapshot());
    assert_eq!(
        (s1.write_backpressure, s1.write_backpressure_active),
        (1, 0)
    );
    assert_eq!(
        (s2.write_backpressure, s2.write_backpressure_active),
        (0, 1)
    );
    assert_eq!((s1.write_buffered, s2.write_buffered), (0, 32));

    drop(io);
    sleep(Millis(10)).await;
    let (s1, s2) = (m1.snapshot(), m2.snapshot());
    assert_eq!(s2.write_backpressure_active, 0);
    assert_eq!(s2.write_buffered, 0);
    assert_eq!((s1.closed.total, s2.closed.total), (0, 1));
}

/// STATEMENT: Clearing a filterless read source releases backpressure and resumes transport input.
///
/// HOW:
/// - Fill the read watermark, clear input through `with_read_src`, and check pause/pressure flags clear.
/// - Send further bytes and verify read readiness returns.
#[ntex::test]
async fn read_src_releases_read_backpressure() {
    let (client, server) = IoTest::create();

    let io = Io::new(
        server,
        SharedCfg::new("SRV").add(
            IoConfig::default()
                .set_read_size(BytePageSize::Size4, BytePageSize::Size4)
                .set_read_backpressure(BytePageSize::Size4.capacity()),
        ),
    );
    assert!(lazy(|cx| io.poll_read_more(cx)).await.is_pending());

    client.write(vec![b'x'; BytePageSize::Size4.capacity()]);
    sleep(Millis(50)).await;
    assert!(io.flags().is_rd_backpressure());

    // On a filterless Io the transport-facing source aliases the
    // application-facing read destination.
    let len = io.get_ref().with_read_src(|buf| {
        let len = buf.len();
        buf.clear();
        len
    });
    assert!(len > 0);
    assert!(!io.flags().is_rd_backpressure());
    assert!(!io.flags().is_read_paused());

    // reads resume
    client.write(BIN2);
    sleep(Millis(50)).await;
    assert!(io.flags().is_read_ready());
}

/// STATEMENT: Clearing application input through filter-buffer access releases read backpressure.
///
/// HOW:
/// - Fill the read watermark, clear the destination through `with_buf`, and check reads unpause.
/// - Send additional bytes and verify read readiness resumes.
#[ntex::test]
async fn with_buf_releases_read_backpressure() {
    let (client, server) = IoTest::create();

    let io = Io::new(
        server,
        SharedCfg::new("SRV").add(
            IoConfig::default()
                .set_read_size(BytePageSize::Size4, BytePageSize::Size4)
                .set_read_backpressure(BytePageSize::Size4.capacity()),
        ),
    );
    assert!(lazy(|cx| io.poll_read_more(cx)).await.is_pending());

    client.write(vec![b'x'; BytePageSize::Size4.capacity()]);
    sleep(Millis(50)).await;
    assert!(io.flags().is_rd_backpressure());

    let len = io
        .get_ref()
        .with_buf(|buf| {
            buf.with_read_buffers(|_, dst| {
                let len = dst.len();
                dst.clear();
                len
            })
        })
        .unwrap();
    assert!(len > 0);
    assert!(!io.flags().is_rd_backpressure());
    assert!(!io.flags().is_read_paused());

    // reads resume
    client.write(BIN2);
    sleep(Millis(50)).await;
    assert!(io.flags().is_read_ready());
}

/// STATEMENT: A cancelled full flush does not suppress partial-flush wakeups.
///
/// HOW:
/// - Poll a full flush under backpressure, abandon it, and register a partial flush.
/// - Drain below the release threshold without emptying output and verify the partial flush wakes and completes.
#[ntex::test]
async fn cancelled_full_flush_keeps_partial_flush_wake() {
    let io = Io::new(
        IoTest::create().0,
        SharedCfg::new("SRV").add(IoConfig::default().set_write_backpressure(8)),
    );
    let ctx = IoContext::new(io.get_ref());

    io.with_write_src(|buf| buf.put_slice(b"12345678")).unwrap();
    assert!(io.st().flags.is_wr_backpressure());
    // full flush is polled and then dropped
    assert!(lazy(|cx| io.poll_flush(cx, true)).await.is_pending());
    assert!(io.st().flags.is_write_flush());

    assert!(lazy(|cx| io.poll_flush(cx, false)).await.is_pending());
    assert!(io.st().dispatch_task.is_set());

    // output drops below the release threshold, but is not drained
    Iops::run();
    assert_eq!(ctx.with_write_dst(|buf| buf.split_to(5).freeze()), b"12345");
    assert_eq!(ctx.update_write_status(Ok(5)), IoTaskStatus::Io);
    assert!(!io.st().dispatch_task.is_set());
    assert!(lazy(|cx| io.poll_flush(cx, false)).await.is_ready());
}

/// STATEMENT: Write scheduling, backpressure, and flush readiness follow queued-output progress.
///
/// HOW:
/// - Queue bytes across the watermark, run scheduled writes, and drain output in stages.
/// - Check task wakes, pressure notifications, partial/full flush flags, and termination errors.
#[ntex::test]
async fn write() {
    let io = Io::new(
        IoTest::create().0,
        SharedCfg::new("SRV").add(IoConfig::default().set_write_backpressure(8)),
    );
    assert!(lazy(|cx| io.poll_status_update(cx)).await.is_pending());
    assert!(io.st().dispatch_task.is_set());
    assert!(io.st().flags.is_direct_wr_enabled());

    let ctx = IoContext::new(io.get_ref());

    // == No write work
    assert_eq!(lazy(|cx| ctx.poll_write_ready(cx)).await, Poll::Pending);
    assert!(io.st().write_task.is_set());
    assert!(io.st().flags.is_write_paused());
    assert!(!io.st().flags.is_wr_backpressure());

    // write
    io.with_write_src(|buf| buf.put_slice(b"1234")).unwrap();
    assert_eq!(lazy(|cx| ctx.poll_write_ready(cx)).await, Poll::Pending);
    // write task is paused
    assert!(io.st().flags.is_write_paused());
    // send-buf op is scheduled
    assert!(io.st().flags.is_wr_send_scheduled());
    // back-pressure is not enabled
    assert!(!io.st().flags.is_wr_backpressure());
    // dispatch is not woken up
    assert!(io.st().dispatch_task.is_set());

    // == enable wr backpressure
    io.with_write_src(|buf| buf.put_slice(b"5678")).unwrap();
    // back-pressure is enabled
    assert!(io.st().flags.is_wr_backpressure());
    // dispatch is woken up
    assert!(!io.st().dispatch_task.is_set());
    // write task is set
    assert!(io.st().write_task.is_set());
    // dispatcher gets WriteBackpressure
    assert!(matches!(
        lazy(|cx| io.poll_status_update(cx)).await,
        Poll::Ready(IoStatusUpdate::WriteBackpressure)
    ));
    // flush write buffer
    assert!(lazy(|cx| io.poll_flush(cx, false)).await.is_pending());
    // full flush is not enabled
    assert!(!io.st().flags.is_write_flush());

    // run send-buf ops
    Iops::run();
    // send-buf op is not scheduled
    assert!(!io.st().flags.is_wr_send_scheduled());
    // write task is not paused
    assert!(!io.st().flags.is_write_paused());
    // write task has been woken up
    assert!(!io.st().write_task.is_set());
    // write task can proceed
    assert_eq!(
        lazy(|cx| ctx.poll_write_ready(cx)).await,
        Poll::Ready(Readiness::Ready)
    );

    // wrote 4 bytes to io
    assert_eq!(ctx.with_write_dst(|buf| buf.split_to(4).freeze()), b"1234");
    // continue to write
    assert_eq!(ctx.update_write_status(Ok(4)), IoTaskStatus::Io);
    // write task can proceed
    assert_eq!(
        lazy(|cx| ctx.poll_write_ready(cx)).await,
        Poll::Ready(Readiness::Ready)
    );
    // write task is not paused
    assert!(!io.st().flags.is_write_paused());
    // back-pressure is enabled
    assert!(io.st().flags.is_wr_backpressure());
    // the write buf dropped below the release threshold, back-pressure is
    // released and no further WriteBackpressure is reported
    assert!(lazy(|cx| io.poll_status_update(cx)).await.is_pending());
    // back-pressure is disabled
    assert!(!io.st().flags.is_wr_backpressure());
    assert!(lazy(|cx| io.poll_status_update(cx)).await.is_pending());
    // write buffer is flushed
    assert!(matches!(
        lazy(|cx| io.poll_flush(cx, false)).await,
        Poll::Ready(Ok(()))
    ));

    // full flush write buffer
    io.with_write_src(|buf| buf.put_slice(b"1234")).unwrap();
    assert!(lazy(|cx| io.poll_flush(cx, true)).await.is_pending());
    // full flush is enabled
    assert!(io.st().flags.is_write_flush());
    // back-pressure is enabled
    assert!(io.st().flags.is_wr_backpressure());

    // wrote all data
    Iops::run();
    assert_eq!(ctx.with_write_dst(BytePages::freeze), b"56781234");
    // write task is not paused, so send-buf op is not scheduled
    assert!(!io.st().flags.is_wr_send_scheduled());
    // update status, no more work
    assert_eq!(ctx.update_write_status(Ok(8)), IoTaskStatus::Pause);
    // write task is paused
    assert!(io.st().flags.is_write_paused());
    // flush is still enabled
    assert!(io.st().flags.is_write_flush());
    // back-pressure is still enabled
    assert!(io.st().flags.is_wr_backpressure());
    // dispatch is woken up
    assert!(!io.st().dispatch_task.is_set());

    // write buffer is flushed
    assert!(matches!(
        lazy(|cx| io.poll_flush(cx, false)).await,
        Poll::Ready(Ok(()))
    ));
    // full flush is disabled
    assert!(!io.st().flags.is_write_flush());
    // back-pressure is disabled
    assert!(!io.st().flags.is_wr_backpressure());

    // == Terminate
    io.terminate();
    // read task is woken
    assert!(!io.st().write_task.is_set());
    // write task ready
    assert_eq!(
        lazy(|cx| ctx.poll_write_ready(cx)).await,
        Poll::Ready(Readiness::Terminate)
    );
    // flush returns error
    let Poll::Ready(Err(err)) = lazy(|cx| io.poll_flush(cx, false)).await else {
        panic!()
    };
    assert_eq!(err.kind(), io::ErrorKind::NotConnected);
    // statis returns error
    assert!(matches!(
        lazy(|cx| io.poll_status_update(cx)).await,
        Poll::Ready(IoStatusUpdate::PeerGone(None))
    ));
}

/// STATEMENT: Local graceful shutdown reports a peer-gone status without an error.
///
/// HOW:
/// - Shut down without peer activity and verify inactive state and `IoStatusUpdate::PeerGone(None)`.
#[ntex::test]
async fn local_shutdown_reports_peer_gone_without_error() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);
    let io = Io::from(server);

    // purely local graceful shutdown, the peer does nothing
    io.shutdown().await.unwrap();

    assert!(!io.is_active());
    assert!(matches!(
        lazy(|cx| io.poll_status_update(cx)).await,
        Poll::Ready(IoStatusUpdate::PeerGone(None))
    ));
}

/// STATEMENT: Replacing configuration enables or disables eager writes according to the new threshold.
///
/// HOW:
/// - Start with eager writes disabled, set a positive threshold, then restore zero.
/// - Check both stored thresholds and direct-write flags after each change.
#[ntex::test]
async fn set_config_updates_eager_write_support() {
    let io = Io::new(
        IoTest::create().0,
        SharedCfg::new("SRV").add(IoConfig::new().set_write_buf_threshold(0)),
    );
    assert!(!io.st().flags.is_direct_wr_enabled());

    // SAFETY: no reference returned by `io.cfg()` is retained.
    unsafe {
        io.set_config(SharedCfg::new("SRV").add(IoConfig::new().set_write_buf_threshold(1024)));
    }
    assert!(io.st().flags.is_direct_wr_enabled());
    assert_eq!(io.cfg().write_buf_threshold(), 1024);

    // SAFETY: the previous `io.cfg()` reference was limited to the
    // assertion statement and is no longer live.
    unsafe {
        io.set_config(SharedCfg::new("SRV").add(IoConfig::new().set_write_buf_threshold(0)));
    }
    assert!(!io.st().flags.is_direct_wr_enabled());
    assert_eq!(io.cfg().write_buf_threshold(), 0);
}

/// STATEMENT: Successful eager writes use the drained buffer size when deciding pressure and scheduling.
///
/// HOW:
/// - Encode above the pressure watermark using a transport that immediately consumes all output.
/// - Verify zero queued bytes, paused writes, and no pressure or scheduled-write flag.
#[ntex::test]
async fn eager_write_uses_updated_buffer_size() {
    #[derive(Debug)]
    struct DirectWrite;

    impl IoStream for DirectWrite {
        fn start(self, _: IoContext) -> Box<dyn Handle> {
            Box::new(self)
        }
    }

    impl Handle for DirectWrite {
        fn write(&self, ctx: &IoContext) {
            let n = ctx.with_write_dst(|buf| {
                let n = buf.len();
                buf.clear();
                n
            });
            let _ = ctx.update_write_status(Ok(n));
        }
    }

    let io = Io::new(
        DirectWrite,
        SharedCfg::new("SRV").add(
            IoConfig::new()
                .set_write_buf_threshold(1)
                .set_write_backpressure(8),
        ),
    );

    io.encode_slice(BIN2).unwrap();

    assert_eq!(io.st().buffer.write_buf_size(), 0);
    assert!(io.flags().is_write_paused());
    assert!(!io.flags().is_wr_backpressure());
    assert!(!io.st().flags.is_wr_send_scheduled());
}

/// STATEMENT: Eager encoding propagates transport write errors and starts termination.
///
/// HOW:
/// - Encode through a direct-write transport that reports a connection reset.
/// - Check the returned error kind/message and terminating state.
#[ntex::test]
async fn eager_write_reports_transport_error() {
    #[derive(Debug)]
    struct FailedWrite;

    impl IoStream for FailedWrite {
        fn start(self, _: IoContext) -> Box<dyn Handle> {
            Box::new(self)
        }
    }

    impl Handle for FailedWrite {
        fn write(&self, ctx: &IoContext) {
            ctx.update_write_status(Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "connection reset",
            )));
        }
    }

    let io = Io::new(
        FailedWrite,
        SharedCfg::new("SRV").add(IoConfig::new().set_write_buf_threshold(1)),
    );

    let err = io.encode_slice(BIN2).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::ConnectionReset);
    assert_eq!(err.to_string(), "connection reset");
    assert!(io.st().flags.is_terminating());
}

/// STATEMENT: Termination triggered during an eager write releases the temporarily borrowed transport handle.
///
/// HOW:
/// - Encode through a drop-tracked transport that reports a reset from its write callback.
/// - Verify terminating state, handle destruction, and no handle reinstalled in connection state.
#[ntex::test]
async fn terminate_during_eager_write_releases_transport() {
    #[derive(Debug)]
    struct FailedWrite(Rc<Cell<bool>>);

    impl Drop for FailedWrite {
        fn drop(&mut self) {
            self.0.set(true);
        }
    }

    impl IoStream for FailedWrite {
        fn start(self, _: IoContext) -> Box<dyn Handle> {
            Box::new(self)
        }
    }

    impl Handle for FailedWrite {
        fn write(&self, ctx: &IoContext) {
            ctx.update_write_status(Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "connection reset",
            )));
        }
    }

    let dropped = Rc::new(Cell::new(false));
    let io = Io::new(
        FailedWrite(dropped.clone()),
        SharedCfg::new("SRV").add(IoConfig::new().set_write_buf_threshold(1)),
    );

    io.encode_slice(BIN2).unwrap_err();
    assert!(io.st().flags.is_terminating());

    // the handle is taken for the duration of the direct write, so the
    // terminate it triggered could not release the transport itself
    assert!(dropped.get());
    assert!(io.st().handle.take().is_none());
}

/// STATEMENT: A transport query that stops the connection does not reinstall or leak its handle.
///
/// HOW:
/// - Query a transport that reports stopped from the query callback and retains its context.
/// - Check closure and handle destruction, then drop `Io` and verify the weak state reference expires.
#[ntex::test]
async fn stop_during_query_releases_transport() {
    #[derive(Debug)]
    struct QueryStop {
        ctx: RefCell<Option<IoContext>>,
        dropped: Rc<Cell<bool>>,
    }

    impl Drop for QueryStop {
        fn drop(&mut self) {
            self.dropped.set(true);
        }
    }

    impl IoStream for QueryStop {
        fn start(self, ctx: IoContext) -> Box<dyn Handle> {
            *self.ctx.borrow_mut() = Some(ctx);
            Box::new(self)
        }
    }

    impl Handle for QueryStop {
        fn query(&self, _: std::any::TypeId) -> Option<Box<dyn std::any::Any>> {
            self.ctx.borrow().as_ref().unwrap().stopped(None);
            None
        }
    }

    let dropped = Rc::new(Cell::new(false));
    let io = Io::new(
        QueryStop {
            ctx: RefCell::new(None),
            dropped: dropped.clone(),
        },
        SharedCfg::default(),
    );
    let st = Rc::downgrade(&io.get_ref().0);

    assert!(io.query::<u8>().as_ref().is_none());
    assert!(io.is_closed());
    assert!(dropped.get(), "handle was reinstalled");
    drop(io);
    assert!(st.upgrade().is_none(), "connection state leaked");
}

/// STATEMENT: Draining queued writes releases backpressure without a new status notification.
///
/// HOW:
/// - Block peer writes, queue a payload above the watermark, then allow the peer to drain it.
/// - Verify payload delivery, pending status polling, cleared pressure, and successful partial flush.
#[ntex::test]
async fn write_backpressure() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(0);

    let io = Io::new(
        server,
        SharedCfg::new("SRV").add(IoConfig::default().set_write_backpressure(16)),
    );
    assert!(lazy(|cx| io.poll_read_more(cx)).await.is_pending());
    assert!(io.flags().is_write_paused());
    assert!(!io.flags().is_wr_backpressure());
    assert!(!io.is_wr_backpressure());

    io.encode_slice(BIN2).unwrap();
    assert!(Iops::is_registered(&io));
    assert!(io.flags().is_wr_backpressure());

    client.remote_buffer_cap(1024);
    let item = client.read().await.unwrap();
    assert_eq!(item, BIN2);
    assert!(io.flags().is_wr_backpressure());
    // the write buf drained, back-pressure is released and no status
    // update is reported, same as poll_flush() below
    assert!(lazy(|cx| io.poll_status_update(cx)).await.is_pending());
    assert!(!io.flags().is_wr_backpressure());
    assert!(matches!(
        lazy(|cx| io.poll_flush(cx, false)).await,
        Poll::Ready(Ok(()))
    ));
    assert!(!io.flags().is_wr_backpressure());
}

/// STATEMENT: Partial flush releases write backpressure only at the half-watermark.
///
/// HOW:
/// - Queue eight bytes, report one written byte, and verify flush still waits under pressure.
/// - Report three more written bytes and check flush succeeds with pressure cleared.
#[ntex::test]
async fn partial_flush_keeps_write_backpressure_until_half_watermark() {
    let io = Io::new(
        IoTest::create().0,
        SharedCfg::new("SRV").add(IoConfig::default().set_write_backpressure(8)),
    );
    let ctx = IoContext::new(io.get_ref());

    io.encode_slice(b"12345678").unwrap();
    assert!(io.flags().is_wr_backpressure());

    assert_eq!(ctx.with_write_dst(|buf| buf.split_to(1).len()), 1);
    assert_eq!(ctx.update_write_status(Ok(1)), IoTaskStatus::Io);
    assert!(lazy(|cx| io.poll_flush(cx, false)).await.is_pending());
    assert!(io.flags().is_wr_backpressure());

    assert_eq!(ctx.with_write_dst(|buf| buf.split_to(3).len()), 3);
    assert_eq!(ctx.update_write_status(Ok(3)), IoTaskStatus::Io);
    assert!(matches!(
        lazy(|cx| io.poll_flush(cx, false)).await,
        Poll::Ready(Ok(()))
    ));
    assert!(!io.flags().is_wr_backpressure());
}

/// STATEMENT: Write progress wakes all producers at the release threshold without dispatcher polling.
///
/// HOW:
/// - Register two write-ready producers while eight bytes are outstanding.
/// - Drain first above, then to the half-watermark and check both producers resume only at the threshold.
#[ntex::test]
async fn write_ready_waits_for_release_threshold() {
    let io = Io::new(
        IoTest::create().0,
        SharedCfg::new("SRV").add(IoConfig::default().set_write_backpressure(8)),
    );
    let ctx = IoContext::new(io.get_ref());

    // no back-pressure
    assert!(io.write_ready().await.is_ok());

    io.encode_slice(b"12345678").unwrap();
    assert!(io.flags().is_wr_backpressure());

    // several producers wait at once
    let done = Rc::new(Cell::new(0));
    for _ in 0..2 {
        let (io, done) = (io.get_ref(), done.clone());
        ntex_util::spawn(async move {
            io.write_ready().await.unwrap();
            done.set(done.get() + 1);
        });
    }
    sleep(Millis(10)).await;
    assert_eq!(done.get(), 0);

    // above the release threshold
    assert_eq!(ctx.with_write_dst(|buf| buf.split_to(1).len()), 1);
    assert_eq!(ctx.update_write_status(Ok(1)), IoTaskStatus::Io);
    sleep(Millis(10)).await;
    assert_eq!(done.get(), 0);

    // the write task wakes the producers, the dispatcher does not run
    assert_eq!(ctx.with_write_dst(|buf| buf.split_to(3).len()), 3);
    assert_eq!(ctx.update_write_status(Ok(3)), IoTaskStatus::Io);
    sleep(Millis(10)).await;
    assert_eq!(done.get(), 2);
    assert!(io.flags().is_wr_backpressure());
    assert!(io.write_ready().await.is_ok());
}

/// STATEMENT: A pending full flush does not prevent producer readiness at the half-watermark.
///
/// HOW:
/// - Queue eight bytes, poll a full flush, and start a write-ready producer.
/// - Report four bytes written and verify the producer resumes before output is fully drained.
#[ntex::test]
async fn write_ready_released_during_full_flush() {
    let io = Io::new(
        IoTest::create().0,
        SharedCfg::new("SRV").add(IoConfig::default().set_write_backpressure(8)),
    );
    let ctx = IoContext::new(io.get_ref());

    io.encode_slice(b"12345678").unwrap();
    assert!(lazy(|cx| io.poll_flush(cx, true)).await.is_pending());
    assert!(io.flags().is_write_flush());

    let done = Rc::new(Cell::new(false));
    let (io2, done2) = (io.get_ref(), done.clone());
    ntex_util::spawn(async move {
        io2.write_ready().await.unwrap();
        done2.set(true);
    });
    sleep(Millis(10)).await;
    assert!(!done.get());

    assert_eq!(ctx.with_write_dst(|buf| buf.split_to(4).len()), 4);
    assert_eq!(ctx.update_write_status(Ok(4)), IoTaskStatus::Io);
    sleep(Millis(10)).await;
    assert!(done.get());
}

/// STATEMENT: Termination fails both pending and subsequent write-ready requests.
///
/// HOW:
/// - Stall output above the watermark and register a waiting producer.
/// - Terminate the connection and verify the pending request and a new request return errors.
#[ntex::test]
async fn write_ready_fails_on_disconnect() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(0);
    let io = Io::new(
        server,
        SharedCfg::new("SRV").add(IoConfig::default().set_write_backpressure(8)),
    );
    io.encode_slice(b"12345678").unwrap();
    assert!(io.flags().is_wr_backpressure());

    let res = Rc::new(Cell::new(None));
    let (io2, res2) = (io.get_ref(), res.clone());
    ntex_util::spawn(async move {
        res2.set(Some(io2.write_ready().await.is_err()));
    });
    sleep(Millis(10)).await;
    assert_eq!(res.get(), None);

    io.terminate();
    sleep(Millis(10)).await;
    assert_eq!(res.get(), Some(true));
    assert!(io.write_ready().await.is_err());
}

/// STATEMENT: Tagged waiters preserve registrations across ownership changes and wake on disconnection.
///
/// HOW:
/// - Register multiple tags, wake selectively, drop a waiter, and convert another to static ownership.
/// - Check cloning does not duplicate registration and shutdown wakes and removes remaining waiters.
#[ntex::test]
async fn waiter() {
    let (client, server) = IoTest::create();
    let io = Io::from(server);
    let (mut s1, mut s2, mut s3) = (Waiter::new(&io, 7), Waiter::new(&io, 7), io.waiter(8));
    assert!(lazy(|cx| Pin::new(&mut s1).poll(cx)).await.is_pending());
    assert!(lazy(|cx| Pin::new(&mut s2).poll(cx)).await.is_pending());
    assert!(lazy(|cx| Pin::new(&mut s3).poll(cx)).await.is_pending());
    assert!(lazy(|cx| Pin::new(&mut s1).poll(cx)).await.is_pending());
    assert_eq!(io.get_ref().0.extensions.wakers_len(), 3);

    io.wake(7);
    assert!(lazy(|cx| Pin::new(&mut s1).poll(cx)).await.is_ready());
    assert!(lazy(|cx| Pin::new(&mut s2).poll(cx)).await.is_ready());
    assert!(lazy(|cx| Pin::new(&mut s3).poll(cx)).await.is_pending());
    assert_eq!(io.get_ref().0.extensions.wakers_len(), 1);

    drop(s3);
    assert_eq!(io.get_ref().0.extensions.wakers_len(), 0);

    // a static waiter keeps the registration
    assert!(lazy(|cx| Pin::new(&mut s1).poll(cx)).await.is_pending());
    let mut s1 = s1.into_static();
    assert_eq!(io.get_ref().0.extensions.wakers_len(), 1);

    // a cloned waiter is not registered
    let s4 = s1.clone();
    assert_eq!(io.get_ref().0.extensions.wakers_len(), 1);
    drop(s4);
    assert_eq!(io.get_ref().0.extensions.wakers_len(), 1);

    // a waiter is woken on disconnect
    let res = Rc::new(Cell::new(false));
    let res2 = res.clone();
    ntex_util::spawn(async move {
        (&mut s1).await;
        res2.set(true);
    });
    sleep(Millis(10)).await;
    assert_eq!(io.get_ref().0.extensions.wakers_len(), 1);
    client.close().await;
    timeout(Millis(1000), io.shutdown())
        .await
        .expect("stream shutdown did not complete")
        .unwrap();
    sleep(Millis(10)).await;
    assert!(res.get(), "waiter was not woken on disconnect");
    (&mut s2).await;
    drop(s2);
    assert_eq!(io.get_ref().0.extensions.wakers_len(), 0);
}

/// STATEMENT: Waiters report registered wakes once and re-register on the following poll.
///
/// HOW:
/// - Wake before registration, poll repeatedly, then wake between polls and inspect registration counts.
/// - Verify one ready poll, subsequent re-registration, and cleanup after awaiting another wake.
#[ntex::test]
async fn waiter_poll_ready() {
    let (_client, server) = IoTest::create();
    let io = Io::from(server);
    let waiter = io.waiter(3);
    let ext = &io.get_ref().0.extensions;

    // not registered, the wake is missed
    io.wake(3);
    assert!(lazy(|cx| waiter.poll_ready(cx)).await.is_pending());
    assert!(lazy(|cx| waiter.poll_ready(cx)).await.is_pending());
    assert_eq!(ext.wakers_len(), 1);

    // a wake between polls is reported once, then the waiter registers again
    io.wake(3);
    assert!(lazy(|cx| waiter.poll_ready(cx)).await.is_ready());
    assert_eq!(ext.wakers_len(), 0);
    assert!(lazy(|cx| waiter.poll_ready(cx)).await.is_pending());
    assert_eq!(ext.wakers_len(), 1);

    io.wake(3);
    waiter.await;
    assert_eq!(ext.wakers_len(), 0);
}

/// STATEMENT: Application tag wakes cannot trigger reserved disconnect or write notifications.
///
/// HOW:
/// - Register a disconnect waiter, wake both reserved tags, and check it remains pending and registered.
#[ntex::test]
async fn wake_reserved_tag_is_ignored() {
    let (_client, server) = IoTest::create();
    let io = Io::from(server);
    let mut waiter = io.on_disconnect();
    assert!(lazy(|cx| Pin::new(&mut waiter).poll(cx)).await.is_pending());
    io.wake(TAG_DISCONNECT);
    io.wake(TAG_WRITE);
    assert!(lazy(|cx| Pin::new(&mut waiter).poll(cx)).await.is_pending());
    assert_eq!(io.get_ref().0.extensions.wakers_len(), 1);
}

/// STATEMENT: Debug checks reject constructing an application waiter with the disconnect tag.
///
/// HOW:
/// - Pass `TAG_DISCONNECT` to `Waiter::new` and expect the reserved-tag panic.
#[cfg(debug_assertions)]
#[ntex::test]
#[should_panic(expected = "reserved")]
async fn waiter_reserved_tag() {
    let (_client, server) = IoTest::create();
    let io = Io::from(server);
    let _waiter = Waiter::new(&io, TAG_DISCONNECT);
}

/// STATEMENT: Debug checks reject constructing an application waiter with the write tag.
///
/// HOW:
/// - Pass `TAG_WRITE` to the connection's waiter factory and expect the reserved-tag panic.
#[cfg(debug_assertions)]
#[ntex::test]
#[should_panic(expected = "reserved")]
async fn waiter_reserved_tag_ioref() {
    let (_client, server) = IoTest::create();
    let io = Io::from(server);
    let _waiter = io.waiter(TAG_WRITE);
}

/// STATEMENT: Dropping or waking disconnect and write-ready waiters removes their registrations.
///
/// HOW:
/// - Repeatedly register and drop both waiter kinds and verify counts return to zero.
/// - Register both together, terminate, and check write and disconnect registrations clear at their stages.
#[ntex::test]
async fn dropped_waiters_are_removed() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(0);
    let io = Io::new(
        server,
        SharedCfg::new("SRV").add(IoConfig::default().set_write_backpressure(8)),
    );
    let ext = &io.get_ref().0.extensions;

    for _ in 0..4 {
        let waiter = io.on_disconnect();
        assert!(lazy(|cx| waiter.poll_ready(cx)).await.is_pending());
        assert!(lazy(|cx| waiter.poll_ready(cx)).await.is_pending());
        assert_eq!(ext.wakers_len(), 1);
    }
    assert_eq!(ext.wakers_len(), 0);

    io.encode_slice(b"12345678").unwrap();
    assert!(io.flags().is_wr_backpressure());
    for _ in 0..4 {
        let mut fut = std::pin::pin!(io.write_ready());
        assert!(lazy(|cx| fut.as_mut().poll(cx)).await.is_pending());
        assert!(lazy(|cx| fut.as_mut().poll(cx)).await.is_pending());
        assert_eq!(ext.wakers_len(), 1);
    }
    assert_eq!(ext.wakers_len(), 0);

    // woken waiters are removed
    let waiter = io.on_disconnect();
    assert!(lazy(|cx| waiter.poll_ready(cx)).await.is_pending());
    let mut fut = std::pin::pin!(io.write_ready());
    assert!(lazy(|cx| fut.as_mut().poll(cx)).await.is_pending());
    assert_eq!(ext.wakers_len(), 2);
    io.terminate();
    assert_eq!(ext.wakers_len(), 1);
    assert!(lazy(|cx| fut.as_mut().poll(cx)).await.is_ready());

    // the backend reports the teardown
    sleep(Millis(50)).await;
    assert_eq!(ext.wakers_len(), 0);
    assert!(lazy(|cx| waiter.poll_ready(cx)).await.is_ready());
}

/// STATEMENT: Reusing a woken waiter's slot does not let stale registration IDs remove a replacement.
///
/// HOW:
/// - Alternate registration, waking, and removal of two waiter entries on the same tag.
/// - Check live counts after slot reuse and verify stale removal leaves the replacement registered.
#[test]
fn woken_waiter_slot_is_reused() {
    let ext = Extensions::default();
    let waker = std::task::Waker::noop();
    let (a, b) = (WaiterEntry::new(TAG_WRITE), WaiterEntry::new(TAG_WRITE));

    ext.register_waker(&a, waker);
    ext.wake(TAG_WRITE);

    // `b` reuses the slot of the woken `a`
    ext.register_waker(&b, waker);
    ext.register_waker(&a, waker);
    assert_eq!(ext.wakers_len(), 2);
    ext.remove_waker(&a);
    assert_eq!(ext.wakers_len(), 1);

    // the removed stale id of `a` does not touch `b`
    ext.wake(TAG_WRITE);
    ext.register_waker(&b, waker);
    ext.register_waker(&a, waker);
    ext.wake(TAG_WRITE);
    ext.register_waker(&b, waker);
    ext.remove_waker(&a);
    assert_eq!(ext.wakers_len(), 1);
    ext.remove_waker(&b);
    assert_eq!(ext.wakers_len(), 0);
}

/// STATEMENT: Full flush waits for transport-owned output to be reported written.
///
/// HOW:
/// - Take queued pages into an in-flight write and verify flush waits despite an empty buffer.
/// - Report write completion and check outstanding bytes reach zero and flush completes.
#[ntex::test]
async fn full_flush_waits_for_inflight_write() {
    // A completion based transport takes ownership of the write pages, so
    // the write buffer empties before the bytes reach the peer. A full
    // flush must not complete until they have.
    let io = Io::new(
        IoTest::create().0,
        SharedCfg::new("SRV").add(IoConfig::default()),
    );
    let ctx = IoContext::new(io.get_ref());

    io.encode_slice(b"12345678").unwrap();

    let page = ctx.with_write_dst(BytePages::take).unwrap();
    assert_eq!(page.len(), 8);
    // the buffer is empty, the output is in flight
    assert_eq!(io.st().buffer.write_buf_size(), 0);
    assert_eq!(io.st().write_outstanding(), 8);

    assert!(lazy(|cx| io.poll_flush(cx, true)).await.is_pending());

    // the transport reports the write
    assert_eq!(ctx.update_write_status(Ok(page.len())), IoTaskStatus::Pause);
    assert_eq!(io.st().write_outstanding(), 0);
    assert!(matches!(
        lazy(|cx| io.poll_flush(cx, true)).await,
        Poll::Ready(Ok(()))
    ));
}

/// STATEMENT: In-flight output keeps write backpressure active after buffered pages are taken.
///
/// HOW:
/// - Queue a watermark-sized payload and transfer its page to the transport.
/// - Verify partial flush remains pending until write completion, then succeeds and clears pressure.
#[ntex::test]
async fn write_backpressure_counts_inflight_output() {
    // Output owned by the transport is still outstanding, so it keeps
    // back-pressure in place.
    let io = Io::new(
        IoTest::create().0,
        SharedCfg::new("SRV").add(IoConfig::default().set_write_backpressure(8)),
    );
    let ctx = IoContext::new(io.get_ref());

    io.encode_slice(b"12345678").unwrap();
    assert!(io.flags().is_wr_backpressure());

    let page = ctx.with_write_dst(BytePages::take).unwrap();
    assert_eq!(io.st().buffer.write_buf_size(), 0);

    // nothing reached the peer yet, so back-pressure stays enabled
    assert!(lazy(|cx| io.poll_flush(cx, false)).await.is_pending());
    assert!(io.flags().is_wr_backpressure());

    assert_eq!(ctx.update_write_status(Ok(page.len())), IoTaskStatus::Pause);
    assert!(matches!(
        lazy(|cx| io.poll_flush(cx, false)).await,
        Poll::Ready(Ok(()))
    ));
    assert!(!io.flags().is_wr_backpressure());
}

/// STATEMENT: Transport shutdown waits for in-flight output before requesting closure.
///
/// HOW:
/// - Take an output page, enter transport shutdown, and check write readiness is pending.
/// - Report the page written and verify readiness changes to close.
#[ntex::test]
async fn transport_shutdown_waits_for_inflight_write() {
    // The transport shutdown phase reports `Close` once the output has
    // been drained. Output the transport already owns has not been
    // drained until it is reported as written.
    let io = Io::new(
        IoTest::create().0,
        SharedCfg::new("SRV").add(IoConfig::default()),
    );
    let ctx = IoContext::new(io.get_ref());

    io.encode_slice(b"12345678").unwrap();
    let page = ctx.with_write_dst(BytePages::take).unwrap();

    // enter the transport shutdown phase
    io.st().flags.enter_filters_stopping();
    io.st().filters_stopped();
    assert!(io.st().flags.is_stopping());

    // nothing left to submit, but the output is still in flight
    assert_eq!(lazy(|cx| ctx.poll_write_ready(cx)).await, Poll::Pending);

    assert_eq!(ctx.update_write_status(Ok(page.len())), IoTaskStatus::Pause);
    assert_eq!(
        lazy(|cx| ctx.poll_write_ready(cx)).await,
        Poll::Ready(Readiness::Close)
    );
}

/// STATEMENT: Graceful shutdown flushes pending output even when read backpressure pauses input.
///
/// HOW:
/// - Stall peer writes, queue a response tail, and fill input to the read watermark.
/// - Start shutdown, unblock writes, and verify the entire tail arrives before disconnection.
#[ntex::test]
async fn shutdown_flushes_write_buf_with_read_backpressure() {
    // Graceful shutdown must flush the pending write buffer even if
    // the peer keeps sending data (read backpressure is enabled).
    let (client, server) = IoTest::create();
    // remote side does not accept any data yet, write task stalls
    client.remote_buffer_cap(0);

    let io = Io::new(
        server,
        SharedCfg::new("SRV").add(
            IoConfig::default()
                .set_read_size(BytePageSize::Size4, BytePageSize::Size4)
                .set_read_backpressure(BytePageSize::Size4.capacity())
                .set_shutdown_timeout(ntex_util::time::Seconds(2)),
        ),
    );

    // queue response data; remote is stalled so it stays in the write buffer
    io.encode_slice(b"response-tail").unwrap();
    sleep(Millis(50)).await;
    assert_eq!(io.st().buffer.write_buf_size(), 13);

    // peer keeps sending, crossing the read high watermark,
    // read task gets paused with back-pressure enabled
    client.write(vec![b'x'; BytePageSize::Size4.capacity()]);
    sleep(Millis(50)).await;
    assert!(io.flags().is_read_paused());
    assert!(io.flags().is_rd_backpressure());
    assert_eq!(io.st().buffer.write_buf_size(), 13);

    // start graceful shutdown while the write buffer is not empty
    io.close();
    sleep(Millis(50)).await;

    // peer starts draining the connection
    client.remote_buffer_cap(1024);

    // all previously queued data must be written before io stream is closed.
    // Without the fix graceful shutdown completes immediately (read is paused
    // with back-pressure), dropping the buffered write data, so the read here
    // returns nothing instead of the queued response tail.
    let data = ntex_util::time::timeout(Millis(2000), client.read())
        .await
        .expect("write buffer was dropped during shutdown")
        .unwrap();
    assert_eq!(&data[..], b"response-tail");

    // the connection still closes gracefully afterwards (within the
    // shutdown timeout) instead of hanging
    ntex_util::time::timeout(Millis(4000), io.on_disconnect())
        .await
        .expect("io stream did not disconnect after flush");
}

/// STATEMENT: Peer read EOF leaves the connection writable so a final response can precede shutdown.
///
/// HOW:
/// - Send a request and close peer input, then decode the request and observe EOF without full closure.
/// - Queue a response, shut down, and verify the peer receives the response.
#[ntex::test]
async fn peer_eof_allows_response_before_shutdown() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);
    let io = Io::from(server);

    client.write("request");
    client.close().await;

    assert_eq!(
        timeout(Millis(1000), io.recv(&BytesCodec))
            .await
            .expect("request was not decoded")
            .unwrap(),
        Some(Bytes::from_static(b"request"))
    );
    assert!(
        timeout(Millis(1000), io.recv(&BytesCodec))
            .await
            .expect("EOF was not reported")
            .unwrap()
            .is_none()
    );
    assert!(!io.st().flags.is_closed());

    io.encode(Bytes::from_static(b"response"), &BytesCodec)
        .unwrap();
    timeout(Millis(1000), io.shutdown())
        .await
        .expect("shutdown did not complete")
        .unwrap();

    assert_eq!(
        timeout(Millis(1000), client.read())
            .await
            .expect("response was not flushed")
            .unwrap(),
        b"response"[..]
    );
}

/// STATEMENT: Shutdown and disconnect waiters complete only after the transport reports stopped.
///
/// HOW:
/// - Enter transport shutdown on a dormant transport and verify both waiters remain pending.
/// - Report stopped through the context and check successful shutdown and disconnect readiness.
#[ntex::test]
async fn shutdown_waits_for_transport_stop() {
    #[derive(Debug)]
    struct DormantTransport;

    impl IoStream for DormantTransport {
        fn start(self, _: IoContext) -> Box<dyn Handle> {
            Box::new(self)
        }
    }

    impl Handle for DormantTransport {}

    let io = Io::from(DormantTransport);
    let ctx = IoContext::new(io.get_ref());
    let waiter = io.on_disconnect();
    io.st().flags.enter_transport_shutdown();

    assert!(lazy(|cx| io.poll_shutdown(cx)).await.is_pending());
    assert!(lazy(|cx| waiter.poll_ready(cx)).await.is_pending());

    ctx.stopped(None);
    assert!(matches!(
        lazy(|cx| io.poll_shutdown(cx)).await,
        Poll::Ready(Ok(()))
    ));
    assert!(lazy(|cx| waiter.poll_ready(cx)).await.is_ready());
}

/// STATEMENT: A direct transport stop preserves buffered input despite terminal shutdown predicates.
///
/// HOW:
/// - Receive input while active, report transport stop, and check closed and shutdown-phase predicates.
/// - Verify the previously buffered input remains available.
#[ntex::test]
async fn a_stopped_connection_keeps_its_buffered_input() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);
    client.write(TEXT);

    let io = Io::from(server);
    io.read_more().await.unwrap().unwrap();
    assert_eq!(io.with_read_dst(|buf| buf.len()), BIN.len());

    IoContext::new(io.get_ref()).stopped(None);
    assert!(io.is_closed() && !io.is_active());
    // the shutdown states report as reached even though none was run
    assert!(io.st().flags.is_stopping() && !io.st().flags.is_shutting_down_filters());
    assert_eq!(
        io.with_read_dst(|buf| buf.len()),
        BIN.len(),
        "input received before the transport went away was discarded"
    );
}

/// STATEMENT: Draining final output during transport shutdown wakes the write task to request closure.
///
/// HOW:
/// - Enter transport shutdown with queued bytes and register the write task through readiness polling.
/// - Drain and report all bytes written, then verify the task was woken and its next poll requests close.
#[ntex::test]
async fn transport_shutdown_drain_wakes_write_task() {
    #[derive(Debug)]
    struct DormantTransport;

    impl IoStream for DormantTransport {
        fn start(self, _: IoContext) -> Box<dyn Handle> {
            Box::new(self)
        }
    }

    impl Handle for DormantTransport {}

    let io = Io::from(DormantTransport);
    let ctx = IoContext::new(io.get_ref());

    // enter the transport shutdown phase with output still queued
    io.encode_slice(b"tail").unwrap();
    io.st().flags.enter_filters_stopping();
    io.st().flags.enter_transport_shutdown();
    assert_eq!(io.st().buffer.write_buf_size(), 4);

    // the write task is asked to drain it
    assert!(matches!(
        lazy(|cx| ctx.poll_write_ready(cx)).await,
        Poll::Ready(Readiness::Ready)
    ));
    assert!(io.st().write_task.is_set());

    // the transport writes everything out
    let res = ctx.with_write_dst(|buf| {
        let mut written = 0;
        while let Some(page) = buf.take() {
            written += page.len();
        }
        Ok(written)
    });
    assert_eq!(ctx.update_write_status(res), IoTaskStatus::Pause);
    assert_eq!(io.st().write_outstanding(), 0);

    // `poll_write_ready` reports `Close` only on another poll, so the
    // drain must have woken the write task for the shutdown to complete
    assert!(!io.st().write_task.is_set());
    assert!(matches!(
        lazy(|cx| ctx.poll_write_ready(cx)).await,
        Poll::Ready(Readiness::Close)
    ));
}

/// STATEMENT: Error termination waits for transport stop before completing shutdown and disconnect waiters.
///
/// HOW:
/// - Stop a dormant transport with a reset error and verify both waiters still wait.
/// - Report transport closure and check shutdown returns the reset while disconnect becomes ready.
#[ntex::test]
async fn termination_waits_for_transport_stop() {
    #[derive(Debug)]
    struct DormantTransport;

    impl IoStream for DormantTransport {
        fn start(self, _: IoContext) -> Box<dyn Handle> {
            Box::new(self)
        }
    }

    impl Handle for DormantTransport {}

    let io = Io::from(DormantTransport);
    let ctx = IoContext::new(io.get_ref());
    let waiter = io.on_disconnect();
    ctx.stop(Some(io::Error::new(
        io::ErrorKind::ConnectionReset,
        "connection reset",
    )));

    assert!(io.st().flags.is_terminating());
    assert!(!io.st().flags.is_closed());
    assert!(lazy(|cx| io.poll_shutdown(cx)).await.is_pending());
    assert!(lazy(|cx| waiter.poll_ready(cx)).await.is_pending());

    ctx.stopped(None);
    let Poll::Ready(Err(err)) = lazy(|cx| io.poll_shutdown(cx)).await else {
        panic!("shutdown did not report termination error");
    };
    assert_eq!(err.kind(), io::ErrorKind::ConnectionReset);
    assert!(lazy(|cx| waiter.poll_ready(cx)).await.is_ready());
}

/// STATEMENT: Sending buffered output does not consume the error retained for shutdown.
///
/// HOW:
/// - Begin termination with a connection reset and check `send_buf` returns its kind and message.
/// - Report stopped and verify shutdown returns the same error.
#[ntex::test]
async fn send_buf_retains_termination_error() {
    #[derive(Debug)]
    struct DormantTransport;

    impl IoStream for DormantTransport {
        fn start(self, _: IoContext) -> Box<dyn Handle> {
            Box::new(self)
        }
    }

    impl Handle for DormantTransport {}

    let io = Io::from(DormantTransport);
    let ctx = IoContext::new(io.get_ref());
    ctx.stop(Some(io::Error::new(
        io::ErrorKind::ConnectionReset,
        "connection reset",
    )));

    let err = io.get_ref().send_buf().unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::ConnectionReset);
    assert_eq!(err.to_string(), "connection reset");

    ctx.stopped(None);
    let err = io.shutdown().await.unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::ConnectionReset);
    assert_eq!(err.to_string(), "connection reset");
}

/// STATEMENT: Read-more, notification, and exact-read APIs preserve a connection's termination error.
///
/// HOW:
/// - Stop a dormant transport with a connection reset.
/// - Poll both readiness APIs and perform an exact read, checking the same error kind and message.
#[ntex::test]
async fn read_readiness_reports_termination_error() {
    #[derive(Debug)]
    struct DormantTransport;

    impl IoStream for DormantTransport {
        fn start(self, _: IoContext) -> Box<dyn Handle> {
            Box::new(self)
        }
    }

    impl Handle for DormantTransport {}

    let io = Io::from(DormantTransport);
    let ctx = IoContext::new(io.get_ref());
    ctx.stop(Some(io::Error::new(
        io::ErrorKind::ConnectionReset,
        "connection reset",
    )));

    let Poll::Ready(Err(err)) = lazy(|cx| io.poll_read_more(cx)).await else {
        panic!("read request did not report termination error");
    };
    assert_eq!(err.kind(), io::ErrorKind::ConnectionReset);
    assert_eq!(err.to_string(), "connection reset");

    let Poll::Ready(Err(err)) = lazy(|cx| io.poll_read_notify(cx)).await else {
        panic!("read notification did not report termination error");
    };
    assert_eq!(err.kind(), io::ErrorKind::ConnectionReset);
    assert_eq!(err.to_string(), "connection reset");

    let err = io.read_exact(&mut [0]).await.unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::ConnectionReset);
    assert_eq!(err.to_string(), "connection reset");
}

/// STATEMENT: Read backpressure prevents a pending filter shutdown from requesting transport reads.
///
/// HOW:
/// - Install a filter with pending shutdown, set read pressure, and clear the input-ready flag.
/// - Begin shutdown and verify read readiness remains pending while transport shutdown is reached.
#[ntex::test]
async fn filter_shutdown_is_blocked_by_read_backpressure() {
    #[derive(Debug)]
    struct PendingShutdown;

    impl FilterLayer for PendingShutdown {
        fn process_read_buf(&self, _: &FilterBuf<'_>) -> io::Result<()> {
            Ok(())
        }

        fn process_write_buf(&self, _: &FilterBuf<'_>) -> io::Result<()> {
            Ok(())
        }

        fn shutdown(&self, _: &FilterBuf<'_>) -> io::Result<Poll<()>> {
            Ok(Poll::Pending)
        }
    }

    let (_client, server) = IoTest::create();
    let io = Io::new(server, SharedCfg::new("SRV")).add_filter(PendingShutdown);

    // The dispatcher decoded part of the input: read readiness is cleared,
    // but the buffer is still above the back-pressure release mark.
    io.st().set_read_ready_and_backpressure();
    io.st().flags.unset_read_ready();
    // shutdown clears the read pause and keeps the back-pressure
    assert!(lazy(|cx| io.poll_shutdown(cx)).await.is_pending());

    let ctx = IoContext::new(io.get_ref());
    assert_eq!(lazy(|cx| ctx.poll_read_ready(cx)).await, Poll::Pending);
    assert!(io.st().flags.is_stopping());
}

/// STATEMENT: Output generated during reads traverses intermediate filters to reach the transport.
///
/// HOW:
/// - Place a reply-emitting filter above a passthrough layer and send peer input.
/// - Drive receiving and verify the peer sees the generated reply.
#[ntex::test]
async fn intermediate_filter_output_reaches_transport() {
    // A filter that emits output while processing reads, the way a TLS
    // layer emits handshake records.
    #[derive(Debug)]
    struct Emit;

    impl FilterLayer for Emit {
        fn process_read_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
            buf.with_read_buffers(|src, dst| {
                if let Some(src) = src {
                    dst.extend_from_slice(src);
                    src.clear();
                }
            });
            buf.with_write_buffers(|_, dst| dst.extend_from_slice(b"pong"));
            Ok(())
        }

        fn process_write_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
            buf.with_write_buffers(BytePages::move_to);
            Ok(())
        }

        fn shutdown(&self, _: &FilterBuf<'_>) -> io::Result<Poll<()>> {
            Ok(Poll::Ready(()))
        }
    }

    #[derive(Debug)]
    struct Passthrough;

    impl FilterLayer for Passthrough {
        fn process_read_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
            buf.with_read_buffers(|src, dst| {
                if let Some(src) = src {
                    dst.extend_from_slice(src);
                    src.clear();
                }
            });
            Ok(())
        }

        fn process_write_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
            buf.with_write_buffers(BytePages::move_to);
            Ok(())
        }

        fn shutdown(&self, _: &FilterBuf<'_>) -> io::Result<Poll<()>> {
            Ok(Poll::Ready(()))
        }
    }

    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);

    // Two layers, so the bytes `Emit` writes during read processing land in
    // an intermediate buffer that `Stack::write_buf_size()` does not count.
    // Only the forced write-chain pass moves them out to the transport.
    let io = Io::from(server).add_filter(Passthrough).add_filter(Emit);

    client.write("ping");
    let _ = io.recv(&BytesCodec).await.unwrap();
    sleep(Millis(50)).await;
    assert!(client.read_any().starts_with(b"pong"));
}

/// STATEMENT: Read-generated output pauses reads until written down to the release threshold.
///
/// HOW:
/// - Generate replies up to the watermark and verify reads pause even after pages become in-flight.
/// - Report staged write progress and check reads resume at half; application output alone does not pause them.
#[ntex::test]
async fn read_pauses_while_read_output_is_not_drained() {
    // A filter that answers every input chunk itself, like WebSocket pong
    // replies, without passing anything to the application.
    #[derive(Debug)]
    struct Reply;

    impl FilterLayer for Reply {
        fn process_read_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
            let data = buf.with_read_buffers(|src, _| src.as_mut().map(BytesMut::take));
            if let Some(data) = data {
                buf.with_write_buffers(|_, dst| dst.extend_from_slice(&data));
            }
            Ok(())
        }

        fn process_write_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
            buf.with_write_buffers(BytePages::move_to);
            Ok(())
        }

        fn shutdown(&self, _: &FilterBuf<'_>) -> io::Result<Poll<()>> {
            Ok(Poll::Ready(()))
        }
    }

    // The transport is driven by the test through `IoContext`.
    #[derive(Debug)]
    struct Manual;

    impl IoStream for Manual {
        fn start(self, _: IoContext) -> Box<dyn Handle> {
            Box::new(self)
        }
    }

    impl Handle for Manual {}

    let io = Io::new(
        Manual,
        SharedCfg::new("SRV").add(IoConfig::default().set_write_backpressure(8)),
    )
    .add_filter(Reply);
    let ctx = IoContext::new(io.get_ref());
    let read = |data: &'static [u8]| {
        ctx.with_read_buf(|buf| {
            buf.extend_from_slice(data);
            Poll::Ready(Ok(data.len()))
        })
    };

    // the replies are below the high watermark
    assert_eq!(read(b"ping"), IoTaskStatus::Io);
    assert!(!io.st().flags.is_read_wr_backpressure());

    // the replies reach the high watermark, reads pause
    assert_eq!(read(b"ping"), IoTaskStatus::Pause);
    assert!(io.st().flags.is_read_wr_backpressure());
    assert_eq!(lazy(|cx| ctx.poll_read_ready(cx)).await, Poll::Pending);
    assert!(io.st().read_task.is_set());

    // the transport takes the replies, in-flight output is not drained
    let _ = ctx.with_write_dst(|buf| buf.split_to(8));
    assert_eq!(io.st().write_outstanding(), 8);
    let _ = ctx.update_write_status(Ok(0));
    assert!(io.st().flags.is_read_wr_backpressure());

    // above half of the high watermark reads stay paused
    let _ = ctx.update_write_status(Ok(3));
    assert!(io.st().flags.is_read_wr_backpressure());
    assert!(io.st().read_task.is_set());

    // drained to half, reading resumes
    let _ = ctx.update_write_status(Ok(1));
    assert!(!io.st().flags.is_read_wr_backpressure());
    assert!(!io.st().read_task.is_set());
    assert_eq!(
        lazy(|cx| ctx.poll_read_ready(cx)).await,
        Poll::Ready(Readiness::Ready)
    );

    // output that was not produced by reading does not pause reads
    io.encode_slice(b"12345678").unwrap();
    assert!(io.st().write_outstanding() >= 8);
    assert_eq!(
        lazy(|cx| ctx.poll_read_ready(cx)).await,
        Poll::Ready(Readiness::Ready)
    );
    assert!(!io.st().flags.is_read_wr_backpressure());
}

/// STATEMENT: Held application output does not prolong a pause caused by read-generated handshake output.
///
/// HOW:
/// - Hold application bytes behind a handshake filter, generate a reply, and verify reads pause.
/// - Drain only the reply, check reads resume, then finish the handshake and verify held bytes are released.
#[ntex::test]
async fn read_output_pause_ignores_held_back_output() {
    // Holds application output until a handshake completes, like TLS
    // during a renegotiation.
    #[derive(Debug, Default)]
    struct Reneg(Cell<bool>);

    impl FilterLayer for Reneg {
        fn process_read_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
            let data = buf.with_read_buffers(|src, _| src.as_mut().map(BytesMut::take));
            match data.as_deref() {
                Some(b"hello") => {
                    buf.with_write_buffers(|_, dst| dst.extend_from_slice(b"handshake"));
                }
                Some(b"done") => {
                    self.0.set(true);
                    buf.with_write_buffers(BytePages::move_to);
                }
                _ => (),
            }
            Ok(())
        }

        fn process_write_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
            if self.0.get() {
                buf.with_write_buffers(BytePages::move_to);
            }
            Ok(())
        }

        fn shutdown(&self, _: &FilterBuf<'_>) -> io::Result<Poll<()>> {
            Ok(Poll::Ready(()))
        }
    }

    #[derive(Debug)]
    struct Manual;

    impl IoStream for Manual {
        fn start(self, _: IoContext) -> Box<dyn Handle> {
            Box::new(self)
        }
    }

    impl Handle for Manual {}

    let io = Io::new(
        Manual,
        SharedCfg::new("SRV").add(IoConfig::default().set_write_backpressure(8)),
    )
    .add_filter(Reneg::default());
    let ctx = IoContext::new(io.get_ref());
    let read = |data: &'static [u8]| {
        ctx.with_read_buf(|buf| {
            buf.extend_from_slice(data);
            Poll::Ready(Ok(data.len()))
        })
    };

    // application output is held back by the filter
    io.encode_slice(b"12345678").unwrap();
    assert_eq!(io.st().write_outstanding(), 8);

    // the handshake reply reaches the high watermark, reads pause
    assert_eq!(read(b"hello"), IoTaskStatus::Pause);
    assert!(io.st().flags.is_read_wr_backpressure());

    // the reply drains, the held back output does not keep reads paused
    let _ = ctx.with_write_dst(|buf| buf.split_to(9));
    let _ = ctx.update_write_status(Ok(9));
    assert_eq!(io.st().write_outstanding(), 8);
    assert!(!io.st().flags.is_read_wr_backpressure());
    assert_eq!(
        lazy(|cx| ctx.poll_read_ready(cx)).await,
        Poll::Ready(Readiness::Ready)
    );

    // the handshake completes and the held back output moves
    let _ = read(b"done");
    assert_eq!(ctx.with_write_dst(|buf| buf.len()), 8);
}

/// Holds application output until the peer sends "go", without producing
/// output of its own.
#[derive(Debug, Default)]
struct HoldOutput(Cell<bool>);

impl FilterLayer for HoldOutput {
    fn process_read_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
        let data = buf.with_read_buffers(|src, _| src.as_mut().map(BytesMut::take));
        if data.as_deref() == Some(b"go") {
            self.0.set(true);
        }
        Ok(())
    }

    fn process_write_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
        if self.0.get() {
            buf.with_write_buffers(BytePages::move_to);
        }
        Ok(())
    }
}

#[derive(Debug)]
struct ManualTransport;

impl IoStream for ManualTransport {
    fn start(self, _: IoContext) -> Box<dyn Handle> {
        Box::new(self)
    }
}

impl Handle for ManualTransport {}

/// STATEMENT: Filters holding output pause transport writes until peer input releases the data.
///
/// HOW:
/// - Queue bytes behind `HoldOutput` and verify empty transport output makes write readiness pending.
/// - Deliver the release message and check the write task wakes with all queued bytes available.
#[ntex::test]
async fn held_back_output_pauses_write_task() {
    let io = Io::new(ManualTransport, SharedCfg::new("SRV")).add_filter(HoldOutput::default());
    let ctx = IoContext::new(io.get_ref());

    io.encode_slice(b"1234").unwrap();
    sleep(Millis(10)).await;

    // the transport has nothing to write, the write task pauses
    assert_eq!(
        lazy(|cx| ctx.poll_write_ready(cx)).await,
        Poll::Ready(Readiness::Ready)
    );
    assert_eq!(ctx.with_write_dst(|buf| buf.len()), 0);
    assert_eq!(ctx.update_write_status(Ok(0)), IoTaskStatus::Pause);
    assert!(lazy(|cx| ctx.poll_write_ready(cx)).await.is_pending());

    // input lets the filter release the output, the write task resumes
    let _ = ctx.with_read_buf(|buf| {
        buf.extend_from_slice(b"go");
        Poll::Ready(Ok(2))
    });
    sleep(Millis(10)).await;
    assert!(!io.st().write_task.is_set(), "write task is not woken");
    assert_eq!(
        lazy(|cx| ctx.poll_write_ready(cx)).await,
        Poll::Ready(Readiness::Ready)
    );
    assert_eq!(ctx.with_write_dst(|buf| buf.len()), 4);
    assert_eq!(ctx.update_write_status(Ok(0)), IoTaskStatus::Io);
}

/// STATEMENT: Output queued during an in-flight write is processed rather than mistaken for held output.
///
/// HOW:
/// - Open `HoldOutput`, take one output page, and queue more application bytes while writing.
/// - Complete the first page and check writing continues with the new bytes at the transport.
#[ntex::test]
async fn held_back_output_queued_during_write_is_processed() {
    let io = Io::new(ManualTransport, SharedCfg::new("SRV")).add_filter(HoldOutput::default());
    let ctx = IoContext::new(io.get_ref());
    let _ = ctx.with_read_buf(|buf| {
        buf.extend_from_slice(b"go");
        Poll::Ready(Ok(2))
    });

    // the transport takes the output, more is queued meanwhile
    io.encode_slice(b"1234").unwrap();
    let page = ctx.with_write_dst(BytePages::take).unwrap();
    io.st().flags.unset_write_paused();
    io.st()
        .buffer
        .with_write_src(io.as_ref(), |buf| buf.extend_from_slice(b"5678"));

    // the queued output is not mistaken for held back output
    assert_eq!(ctx.update_write_status(Ok(page.len())), IoTaskStatus::Io);
    assert_eq!(ctx.with_write_dst(|buf| buf.len()), 4);
}

/// STATEMENT: Shutdown reports output a filter layer still retains instead of
/// discarding it silently, and succeeds once the layer has released its output.
///
/// HOW:
/// - Queue four bytes behind `HoldOutput`, which keeps the default `shutdown`,
///   and check `Io::shutdown` fails with a message naming the four lost bytes
///   while the peer receives nothing.
/// - Repeat after the peer sends the release message and check shutdown
///   succeeds with the bytes delivered.
#[ntex::test]
async fn shutdown_reports_output_retained_by_filter() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);
    let io = Io::from(server).add_filter(HoldOutput::default());

    io.encode_slice(b"1234").unwrap();
    let err = timeout(Millis(1000), io.shutdown())
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "filter layer retained 4 unwritten bytes at shutdown"
    );
    assert!(client.read_any().is_empty());

    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);
    let io = Io::from(server).add_filter(HoldOutput::default());
    client.write(b"go");
    io.encode_slice(b"1234").unwrap();
    sleep(Millis(10)).await;
    timeout(Millis(1000), io.shutdown()).await.unwrap().unwrap();
    assert_eq!(client.read_any(), b"1234".as_ref());
}

/// Expands every write into 600 bytes.
#[derive(Debug)]
struct Expand;

impl FilterLayer for Expand {
    fn process_read_buf(&self, _: &FilterBuf<'_>) -> io::Result<()> {
        Ok(())
    }

    fn process_write_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
        buf.with_write_buffers(|src, dst| {
            src.clear();
            dst.extend_from_slice(&[0; 600]);
        });
        Ok(())
    }
}

/// STATEMENT: Filter-expanded output enables write backpressure when transport-visible bytes cross the watermark.
///
/// HOW:
/// - Expand two small writes into 600-byte chunks around a 1,024-byte watermark.
/// - Update write status and verify pressure is enabled only after expansion crosses the limit.
#[ntex::test]
async fn filter_expansion_enables_wr_backpressure() {
    let cfg = SharedCfg::new("SRV").add(IoConfig::default().set_write_backpressure(1024));
    let io = Io::new(ManualTransport, cfg).add_filter(Expand);
    let ctx = IoContext::new(io.get_ref());

    // the transport polls output, the filter grows it past the watermark
    io.encode_slice(b"a").unwrap();
    assert_eq!(ctx.with_write_dst(|buf| buf.len()), 600);
    assert_eq!(ctx.update_write_status(Ok(0)), IoTaskStatus::Io);
    assert!(!io.st().flags.is_wr_backpressure());
    io.encode_slice(b"b").unwrap();
    assert!(!io.st().flags.is_wr_backpressure());
    assert_eq!(ctx.with_write_dst(|buf| buf.len()), 1200);
    assert_eq!(ctx.update_write_status(Ok(0)), IoTaskStatus::Io);
    assert!(io.st().flags.is_wr_backpressure());
    let mut ready = std::pin::pin!(io.write_ready());
    assert!(lazy(|cx| ready.as_mut().poll(cx)).await.is_pending());
}

/// STATEMENT: Expanding newly queued output after a zero-progress completion updates write backpressure.
///
/// HOW:
/// - Take one expanded page in-flight, queue another small write, and report no write progress.
/// - Verify the second expansion raises outstanding bytes to 1,200 and enables pressure.
#[ntex::test]
async fn filter_expansion_after_write_enables_wr_backpressure() {
    let cfg = SharedCfg::new("SRV").add(IoConfig::default().set_write_backpressure(1024));
    let io = Io::new(ManualTransport, cfg).add_filter(Expand);
    let ctx = IoContext::new(io.get_ref());

    // the transport takes the output, more is queued meanwhile
    io.encode_slice(b"a").unwrap();
    let page = ctx.with_write_dst(BytePages::take).unwrap();
    assert_eq!(page.len(), 600);
    io.encode_slice(b"b").unwrap();
    assert!(!io.st().flags.is_wr_backpressure());

    // the write completes without progress, the queued output expands
    assert_eq!(ctx.update_write_status(Ok(0)), IoTaskStatus::Io);
    assert_eq!(io.st().write_outstanding(), 1200);
    assert!(io.st().flags.is_wr_backpressure());
}

/// STATEMENT: Undrainable held output keeps shutdown pending until its deadline closes the connection.
///
/// HOW:
/// - Enter transport shutdown with output blocked by a filter and verify the write task waits.
/// - Wait past the configured deadline and check close readiness and aborted state.
#[ntex::test]
async fn held_back_output_waits_for_shutdown_deadline() {
    let io = Io::new(
        ManualTransport,
        SharedCfg::new("SRV")
            .add(IoConfig::default().set_shutdown_timeout(ntex_util::time::Seconds(1))),
    )
    .add_filter(HoldOutput::default());
    let ctx = IoContext::new(io.get_ref());

    // enter the transport shutdown phase with held back output
    io.encode_slice(b"1234").unwrap();
    io.st().flags.enter_filters_stopping();
    io.st().flags.enter_transport_shutdown();
    sleep(Millis(10)).await;
    assert_eq!(
        lazy(|cx| ctx.poll_write_ready(cx)).await,
        Poll::Ready(Readiness::Ready)
    );
    assert_eq!(ctx.with_write_dst(|buf| buf.len()), 0);
    assert_eq!(ctx.update_write_status(Ok(0)), IoTaskStatus::Pause);

    // the transport can not drain it, the write task waits
    assert!(lazy(|cx| ctx.poll_write_ready(cx)).await.is_pending());

    // the deadline aborts the connection
    sleep(Millis(1100)).await;
    assert_eq!(
        lazy(|cx| ctx.poll_write_ready(cx)).await,
        Poll::Ready(Readiness::Close)
    );
    assert!(io.st().flags.is_aborted());
}

/// STATEMENT: Transport stop immediately discards undeliverable output but retains readable input.
///
/// HOW:
/// - Buffer input and a large output payload, then report a transport failure.
/// - Verify closed state, zero outstanding output, and unchanged input length.
#[ntex::test]
async fn stopped_discards_undeliverable_output() {
    let io = Io::new(ManualTransport, SharedCfg::new("SRV"));
    let ctx = IoContext::new(io.get_ref());
    let _ = ctx.with_read_buf(|buf| {
        buf.extend_from_slice(b"in");
        Poll::Ready(Ok(2))
    });
    io.encode_slice(&vec![0; 1024 * 1024]).unwrap();

    ctx.stopped(Some(io::Error::other("transport failed")));
    assert!(io.is_closed());
    assert_eq!(io.st().write_outstanding(), 0);
    assert_eq!(io.st().buffer.read_dst_size(), 2);
}

/// STATEMENT: Explicit termination discards both filter-held and transport-facing output.
///
/// HOW:
/// - Queue bytes on both sides of `HoldOutput`, verify their combined size, then terminate.
/// - Check no output remains outstanding.
#[ntex::test]
async fn terminate_discards_output_of_all_layers() {
    let io = Io::new(ManualTransport, SharedCfg::new("SRV")).add_filter(HoldOutput::default());
    io.encode_slice(b"held").unwrap();
    io.st()
        .buffer
        .with_write_dst(io.as_ref(), |buf| buf.extend_from_slice(b"wire"));
    assert_eq!(io.st().write_outstanding(), 8);

    io.terminate();
    assert_eq!(io.st().write_outstanding(), 0);
}

/// STATEMENT: Termination inside filter write processing discards output added during the callback.
///
/// HOW:
/// - Use a filter that terminates while borrowing write buffers and appends bytes to both sides.
/// - Trigger processing and verify force-close state with no buffered or outstanding output.
#[ntex::test]
async fn terminate_from_filter_discards_output() {
    #[derive(Debug)]
    struct Abort;

    impl FilterLayer for Abort {
        fn process_read_buf(&self, _: &FilterBuf<'_>) -> io::Result<()> {
            Ok(())
        }

        fn process_write_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
            buf.with_write_buffers(|src, dst| {
                buf.io().terminate();
                dst.extend_from_slice(b"alert");
                src.extend_from_slice(b"rest");
            });
            Ok(())
        }
    }

    let io = Io::new(ManualTransport, SharedCfg::new("SRV")).add_filter(Abort);
    let ctx = IoContext::new(io.get_ref());
    io.encode_slice(b"data").unwrap();

    // the transport polls output, the filter runs and aborts
    assert_eq!(ctx.with_write_dst(|buf| buf.len()), 0);
    assert!(io.st().flags.is_force_closing());
    assert_eq!(io.st().write_outstanding(), 0);
}

/// STATEMENT: Stopping during write-buffer access discards returned bytes and clears in-flight accounting.
///
/// HOW:
/// - Take an output page, report stopped inside the buffer callback, and append replacement bytes.
/// - Release the buffer and verify both outstanding and buffered write counters are zero.
#[ntex::test]
async fn stop_while_write_buffer_in_use_discards_output() {
    let io = Io::new(ManualTransport, SharedCfg::new("SRV"));
    let ctx = IoContext::new(io.get_ref());
    io.encode_slice(b"12345678").unwrap();

    ctx.with_write_dst(|buf| {
        let _page = buf.take();
        ctx.stopped(None);
        buf.extend_from_slice(b"back");
    });
    assert_eq!(io.st().write_outstanding(), 0);
    assert_eq!(io.st().wr_buffered.get(), 0);
}

/// STATEMENT: Explicit read-more and read-notification requests lift read-output backpressure pauses.
///
/// HOW:
/// - Generate enough filter replies to pause reads before each readiness API call.
/// - Verify each call clears the pause, wakes the read task, and restores transport read readiness.
#[ntex::test]
async fn read_more_lifts_read_output_pause() {
    #[derive(Debug)]
    struct Reply;

    impl FilterLayer for Reply {
        fn process_read_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
            let data = buf.with_read_buffers(|src, _| src.as_mut().map(BytesMut::take));
            if let Some(data) = data {
                buf.with_write_buffers(|_, dst| dst.extend_from_slice(&data));
            }
            Ok(())
        }

        fn process_write_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
            buf.with_write_buffers(BytePages::move_to);
            Ok(())
        }

        fn shutdown(&self, _: &FilterBuf<'_>) -> io::Result<Poll<()>> {
            Ok(Poll::Ready(()))
        }
    }

    #[derive(Debug)]
    struct Manual;

    impl IoStream for Manual {
        fn start(self, _: IoContext) -> Box<dyn Handle> {
            Box::new(self)
        }
    }

    impl Handle for Manual {}

    let io = Io::new(
        Manual,
        SharedCfg::new("SRV").add(IoConfig::default().set_write_backpressure(8)),
    )
    .add_filter(Reply);
    let ctx = IoContext::new(io.get_ref());
    let read = |data: &'static [u8]| {
        ctx.with_read_buf(|buf| {
            buf.extend_from_slice(data);
            Poll::Ready(Ok(data.len()))
        })
    };

    // read_more
    assert_eq!(read(b"pingping"), IoTaskStatus::Pause);
    assert_eq!(lazy(|cx| ctx.poll_read_ready(cx)).await, Poll::Pending);
    assert!(lazy(|cx| io.poll_read_more(cx)).await.is_pending());
    assert!(!io.st().flags.is_read_wr_backpressure());
    assert!(!io.st().read_task.is_set());
    assert_eq!(
        lazy(|cx| ctx.poll_read_ready(cx)).await,
        Poll::Ready(Readiness::Ready)
    );

    // read_notify
    assert_eq!(read(b"ping"), IoTaskStatus::Pause);
    assert_eq!(lazy(|cx| ctx.poll_read_ready(cx)).await, Poll::Pending);
    assert!(lazy(|cx| io.poll_read_notify(cx)).await.is_pending());
    assert!(!io.st().flags.is_read_wr_backpressure());
    assert!(!io.st().read_task.is_set());
    assert_eq!(
        lazy(|cx| ctx.poll_read_ready(cx)).await,
        Poll::Ready(Readiness::Ready)
    );
}

/// STATEMENT: Filter shutdown can continue reading despite an earlier read-output backpressure pause.
///
/// HOW:
/// - Generate queued replies that pause reads, then begin a filter shutdown that awaits input.
/// - Check filter-shutdown state, restored read readiness, and continued read processing.
#[ntex::test]
async fn read_pause_on_read_output_does_not_block_filter_shutdown() {
    #[derive(Debug)]
    struct Reply;

    impl FilterLayer for Reply {
        fn process_read_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
            let data = buf.with_read_buffers(|src, _| src.as_mut().map(BytesMut::take));
            if let Some(data) = data {
                buf.with_write_buffers(|_, dst| dst.extend_from_slice(&data));
            }
            Ok(())
        }

        fn process_write_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
            buf.with_write_buffers(BytePages::move_to);
            Ok(())
        }

        fn shutdown(&self, _: &FilterBuf<'_>) -> io::Result<Poll<()>> {
            // waits for input, like a TLS close_notify
            Ok(Poll::Pending)
        }
    }

    #[derive(Debug)]
    struct Manual;

    impl IoStream for Manual {
        fn start(self, _: IoContext) -> Box<dyn Handle> {
            Box::new(self)
        }
    }

    impl Handle for Manual {}

    let io = Io::new(
        Manual,
        SharedCfg::new("SRV").add(IoConfig::default().set_write_backpressure(8)),
    )
    .add_filter(Reply);
    let ctx = IoContext::new(io.get_ref());
    let read = |data: &'static [u8]| {
        ctx.with_read_buf(|buf| {
            buf.extend_from_slice(data);
            Poll::Ready(Ok(data.len()))
        })
    };

    assert_eq!(read(b"pingping"), IoTaskStatus::Pause);
    assert!(io.st().flags.is_read_wr_backpressure());

    // the filters are shutting down and may need input to complete
    assert!(lazy(|cx| io.poll_shutdown(cx)).await.is_pending());
    assert!(io.st().flags.is_stopping_filters());
    assert_eq!(
        lazy(|cx| ctx.poll_read_ready(cx)).await,
        Poll::Ready(Readiness::Ready)
    );
    assert_eq!(read(b"ping"), IoTaskStatus::Io);
}

/// STATEMENT: Peer EOF ends a filter shutdown that can no longer receive input while still flushing output.
///
/// HOW:
/// - Queue output through a perpetually pending shutdown filter and close peer input.
/// - Verify shutdown finishes well before its deadline and the peer receives the buffered bytes.
#[ntex::test]
async fn peer_eof_completes_filter_shutdown() {
    #[derive(Debug)]
    struct PendingShutdown;

    impl FilterLayer for PendingShutdown {
        fn process_read_buf(&self, _: &FilterBuf<'_>) -> io::Result<()> {
            Ok(())
        }

        fn process_write_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
            buf.with_write_buffers(BytePages::move_to);
            Ok(())
        }

        fn shutdown(&self, buf: &FilterBuf<'_>) -> io::Result<Poll<()>> {
            // waits for input that can never arrive after a clean eof
            buf.with_write_buffers(BytePages::move_to);
            Ok(Poll::Pending)
        }
    }

    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);
    let io = Io::new(
        server,
        SharedCfg::new("SRV")
            .add(IoConfig::default().set_shutdown_timeout(ntex_util::time::Seconds(30))),
    )
    .add_filter(PendingShutdown);

    io.encode_slice(b"bye").unwrap();

    // peer closes cleanly, no further input can arrive
    let peer = client.clone();
    drop(client);
    assert!(io.read_more().await.unwrap().is_none());
    assert!(io.st().flags.is_read_eof());

    // the shutdown completes without waiting for the shutdown timeout
    timeout(Millis(1000), io.shutdown())
        .await
        .expect("transport shutdown did not complete")
        .unwrap();
    assert!(io.st().flags.is_closed());

    // buffered output still reached the peer
    assert_eq!(peer.read_any(), Bytes::from_static(b"bye"));
}

/// A filter that forwards input and never finishes its own shutdown.
#[derive(Debug)]
struct StuckShutdown(Cell<bool>);

impl FilterLayer for StuckShutdown {
    fn process_read_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
        buf.with_read_buffers(|src, dst| {
            if let Some(src) = src {
                dst.extend_from_slice(src);
                src.clear();
            }
        });
        Ok(())
    }

    fn process_write_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
        buf.with_write_buffers(BytePages::move_to);
        Ok(())
    }

    fn shutdown(&self, _: &FilterBuf<'_>) -> io::Result<Poll<()>> {
        self.0.set(true);
        Ok(Poll::Pending)
    }
}

/// STATEMENT: Transport shutdown leaves further input unread once filter shutdown has completed.
///
/// HOW:
/// - Receive initial input, stall output, and enter transport shutdown.
/// - Send additional input and verify it remains in the transport while the connection awaits drain.
#[ntex::test]
async fn transport_shutdown_pauses_read_task() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);
    let io = Io::new(server, SharedCfg::new("SRV"));

    client.write("before");
    sleep(Millis(25)).await;
    assert_eq!(
        io.recv(&BytesCodec).await.unwrap().unwrap(),
        b"before".as_ref()
    );

    // stall the output so the transport shutdown phase does not complete
    client.remote_buffer_cap(0);
    io.get_ref().with_write_dst(|b| b.extend_from_slice(b"out"));

    // enter the transport shutdown phase
    io.st().flags.enter_filters_stopping();
    io.st().flags.enter_transport_shutdown();
    io.st().wake_read_task();

    // the filters are done, so input is left in the transport; it is
    // discarded by the transport itself right before it closes
    client.write("after");
    sleep(Millis(50)).await;
    assert!(!io.st().flags.is_closed());
    assert_eq!(client.remote_buffer(|buf| buf.len()), 5);
}

/// Counts the wakeups delivered through the waker it provides.
#[derive(Default)]
struct WakeCounter(std::sync::atomic::AtomicUsize);

impl std::task::Wake for WakeCounter {
    fn wake(self: std::sync::Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &std::sync::Arc<Self>) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

impl WakeCounter {
    fn count(&self) -> usize {
        self.0.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// STATEMENT: Repeated shutdown polls wake transport tasks only when relevant state changes.
///
/// HOW:
/// - Register counting read/write wakers during pending filter shutdown and poll repeatedly without changes.
/// - Verify no wakes, then pause reads and check another shutdown poll resumes them with one wake.
#[ntex::test]
async fn repeated_shutdown_polls_do_not_wake_tasks() {
    use std::{sync::Arc, task::Waker};

    let (_client, server) = IoTest::create();
    let io = Io::new(server, SharedCfg::new("SRV")).add_filter(StuckShutdown(Cell::new(false)));

    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    assert!(io.poll_shutdown(&mut cx).is_pending());
    assert!(io.st().flags.is_stopping_filters());

    // Polling again without any state change must not wake the transport
    // tasks. A runtime that polls the caller spuriously, as compio does
    // for `block_on`, would otherwise be kept busy with wakeups.
    let rd = Arc::new(WakeCounter::default());
    let wr = Arc::new(WakeCounter::default());
    io.st().read_task.register(&Waker::from(rd.clone()));
    io.st().write_task.register(&Waker::from(wr.clone()));
    for _ in 0..3 {
        assert!(io.poll_shutdown(&mut cx).is_pending());
    }
    assert_eq!(rd.count(), 0);
    assert_eq!(wr.count(), 0);

    // Paused reads are resumed, the filter may need the input.
    io.st().flags.set_read_paused();
    assert!(io.poll_shutdown(&mut cx).is_pending());
    assert!(!io.st().flags.is_read_paused());
    assert_eq!(rd.count(), 1);
}

thread_local! {
    static WAKE_HOOK: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
}

/// A waker that runs the installed hook inline, like a waker that polls
/// the woken task synchronously.
struct InlineWake;

impl std::task::Wake for InlineWake {
    fn wake(self: std::sync::Arc<Self>) {
        if let Some(hook) = WAKE_HOOK.with(|slot| slot.borrow_mut().take()) {
            hook();
        }
    }
}

/// Registers an inline waker as the read task, and returns what the
/// state check observed when it was woken.
fn wake_read_task_inline<F>(
    io: &Io<F>,
    check: impl FnOnce(&IoRef) -> bool + 'static,
) -> Rc<Cell<Option<bool>>> {
    let seen = Rc::new(Cell::new(None));
    let (s, ioref) = (seen.clone(), io.get_ref());
    WAKE_HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(move || s.set(Some(check(&ioref))))));
    io.st()
        .read_task
        .register(&std::task::Waker::from(std::sync::Arc::new(InlineWake)));
    seen
}

/// STATEMENT: Read-task wake callbacks observe resumed reads or advanced shutdown state immediately.
///
/// HOW:
/// - Install synchronous wake hooks before source/destination access and filter-shutdown resumption.
/// - Check each hook sees reads unpaused, and a final hook sees transport shutdown already published.
#[ntex::test]
async fn read_task_is_woken_after_state_is_published() {
    let not_paused = |io: &IoRef| !io.0.flags.is_read_paused();

    let (_client, server) = IoTest::create();
    let io = Io::new(server, SharedCfg::new("SRV"));
    io.st().flags.set_read_paused();
    let seen = wake_read_task_inline(&io, not_paused);
    io.with_read_dst(|_| ());
    assert_eq!(seen.get(), Some(true), "read_dst resume");

    io.st().flags.set_read_paused();
    let seen = wake_read_task_inline(&io, not_paused);
    io.with_read_src(|_| ());
    assert_eq!(seen.get(), Some(true), "read_src resume");

    let (_client, server) = IoTest::create();
    let io = Io::new(server, SharedCfg::new("SRV")).add_filter(StuckShutdown(Cell::new(false)));
    let mut cx = Context::from_waker(std::task::Waker::noop());
    assert!(io.poll_shutdown(&mut cx).is_pending());
    io.st().flags.set_read_paused();
    let seen = wake_read_task_inline(&io, not_paused);
    assert!(io.poll_shutdown(&mut cx).is_pending());
    assert_eq!(seen.get(), Some(true), "filter shutdown resume");

    let seen = wake_read_task_inline(&io, |io| io.0.flags.is_stopping());
    io.st().filters_stopped();
    assert_eq!(seen.get(), Some(true), "transport shutdown");
}

/// STATEMENT: Pending filter shutdown does not allow unbounded input buffering.
///
/// HOW:
/// - Begin shutdown with a non-completing filter and repeatedly send input above the read watermark.
/// - Verify local buffering stays within the asserted bound and unread bytes remain at the peer.
#[ntex::test]
async fn filter_shutdown_applies_read_backpressure() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024 * 1024);
    let io = Io::new(
        server,
        SharedCfg::new("SRV").add(
            IoConfig::default()
                .set_read_size(BytePageSize::Size4, BytePageSize::Size4)
                .set_read_backpressure(BytePageSize::Size4.capacity())
                .set_shutdown_timeout(ntex_util::time::Seconds(30)),
        ),
    )
    .add_filter(StuckShutdown(Cell::new(false)));

    let ioref = io.get_ref();
    let high = BytePageSize::Size4.capacity();
    ntex::rt::spawn(async move {
        let _ = io.shutdown().await;
    });
    sleep(Millis(50)).await;

    // The filter never completes, so the connection stays in the filter
    // shutdown phase while the peer keeps sending.
    for _ in 0..40 {
        client.write("A".repeat(1024));
        sleep(Millis(5)).await;
    }
    sleep(Millis(100)).await;

    // Reads are backpressured instead of draining the peer without bound.
    let buffered = ioref.with_read_dst(|buf| buf.len());
    assert!(
        buffered <= high * 2,
        "read buffer grew to {buffered} with a high watermark of {high}"
    );
    assert!(
        client.remote_buffer(|buf| !buf.is_empty()),
        "peer send buffer was drained despite read backpressure"
    );
}

/// STATEMENT: Unconsumed input blocking a filter handshake ends shutdown before its long deadline.
///
/// HOW:
/// - Fill the read watermark through a filter whose shutdown never completes.
/// - Start shutdown and verify it returns an `Other` error within a shorter outer timeout.
#[ntex::test]
async fn filter_shutdown_blocked_by_unconsumed_input() {
    // The dispatcher stops consuming with a full read buffer, so the filter
    // cannot receive the input it is waiting for. The shutdown gives up on
    // the filter handshake promptly instead of reading without bound until
    // the deadline.
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024 * 1024);
    let io = Io::new(
        server,
        SharedCfg::new("SRV").add(
            IoConfig::default()
                .set_read_size(BytePageSize::Size4, BytePageSize::Size4)
                .set_read_backpressure(BytePageSize::Size4.capacity())
                .set_shutdown_timeout(ntex_util::time::Seconds(30)),
        ),
    )
    .add_filter(StuckShutdown(Cell::new(false)));

    client.write("A".repeat(4096));
    sleep(Millis(50)).await;
    assert!(
        io.get_ref().is_rd_backpressure(),
        "read backpressure was not active before the shutdown"
    );

    // Completes well inside the 30 second deadline, so it is the blocked
    // detection that ends the phase rather than the timeout.
    let err = timeout(Millis(3000), io.shutdown())
        .await
        .expect("shutdown did not complete")
        .unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::Other);
}

#[derive(Debug)]
struct Passthrough;

impl FilterLayer for Passthrough {
    fn process_read_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
        buf.with_read_buffers(|src, dst| {
            if let Some(src) = src {
                dst.extend_from_slice(src);
                src.clear();
            }
        });
        Ok(())
    }

    fn process_write_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
        buf.with_write_buffers(BytePages::move_to);
        Ok(())
    }
}

/// A filter that sends "bye" on shutdown and waits for the peer's "ack".
#[derive(Debug, Default)]
struct AckShutdown {
    sent: Cell<bool>,
    acked: Cell<bool>,
}

impl FilterLayer for AckShutdown {
    fn process_read_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
        buf.with_read_buffers(|src, dst| {
            if let Some(src) = src {
                if self.sent.get() && &src[..] == b"ack" {
                    self.acked.set(true);
                } else {
                    dst.extend_from_slice(src);
                }
                src.clear();
            }
        });
        Ok(())
    }

    fn process_write_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
        buf.with_write_buffers(BytePages::move_to);
        Ok(())
    }

    fn shutdown(&self, buf: &FilterBuf<'_>) -> io::Result<Poll<()>> {
        if !self.sent.replace(true) {
            buf.with_write_buffers(|_, dst| dst.extend_from_slice(b"bye"));
        }
        Ok(if self.acked.get() {
            Poll::Ready(())
        } else {
            Poll::Pending
        })
    }
}

async fn filter_shutdown_waits_for_peer<F: Filter>(client: IoTest, io: Io<F>) {
    client.remote_buffer_cap(1024);
    let io = io.add_filter(AckShutdown::default());
    let done = Rc::new(Cell::new(false));
    let done2 = done.clone();
    ntex::rt::spawn(async move {
        io.shutdown().await.unwrap();
        done2.set(true);
    });
    sleep(Millis(50)).await;

    // the shutdown output reached the peer while the filter is pending
    assert_eq!(client.read_any(), Bytes::from_static(b"bye"));
    assert!(!done.get());

    // the peer's input completes the filter shutdown
    client.write("ack");
    sleep(Millis(50)).await;
    assert!(done.get());
}

/// STATEMENT: Filter shutdown delivers its closing message and waits for the peer's acknowledgment.
///
/// HOW:
/// - Shut down through `AckShutdown`, verify the peer receives `bye`, and check shutdown is still pending.
/// - Send `ack` and verify shutdown completes.
#[ntex::test]
async fn filter_shutdown_completes_on_peer_input() {
    let (client, server) = IoTest::create();
    let io = Io::new(server, SharedCfg::new("SRV"));
    filter_shutdown_waits_for_peer(client, io).await;
}

/// STATEMENT: Shutdown handshake output traverses inner filters while the outer shutdown waits for input.
///
/// HOW:
/// - Place `AckShutdown` above a passthrough layer and verify `bye` reaches the peer before completion.
/// - Return `ack` through the layers and check shutdown completes.
#[ntex::test]
async fn filter_shutdown_output_passes_inner_filters() {
    let (client, server) = IoTest::create();
    let io = Io::new(server, SharedCfg::new("SRV")).add_filter(Passthrough);
    filter_shutdown_waits_for_peer(client, io).await;
}

/// STATEMENT: A failing filter's closing output traverses inner layers before connection closure.
///
/// HOW:
/// - Send input through a filter that emits `err`, requests close, and returns an invalid-data error.
/// - Verify receiving reports that error and the peer gets the closing output through a passthrough layer.
#[ntex::test]
async fn filter_failure_output_passes_inner_filters() {
    /// Answers any input with "err", closes and fails.
    #[derive(Debug)]
    struct FailOnInput;

    impl FilterLayer for FailOnInput {
        fn process_read_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
            if buf.with_read_src(|src| src.take().is_some()) {
                buf.with_write_buffers(|_, dst| dst.extend_from_slice(b"err"));
                buf.io().close();
                Err(io::Error::new(io::ErrorKind::InvalidData, "failed"))
            } else {
                Ok(())
            }
        }

        fn process_write_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
            buf.with_write_buffers(BytePages::move_to);
            Ok(())
        }
    }

    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);
    let io = Io::new(server, SharedCfg::new("SRV"))
        .add_filter(Passthrough)
        .add_filter(FailOnInput);

    client.write("input");
    let err = io.recv(&BytesCodec).await.unwrap_err();
    assert_eq!(err.into_inner().kind(), io::ErrorKind::InvalidData);
    sleep(Millis(50)).await;

    assert_eq!(client.read_any(), Bytes::from_static(b"err"));
    assert!(io.is_closed());
}

/// STATEMENT: Filter failure delivers its error record without flushing previously held application data.
///
/// HOW:
/// - Hold application output in a filter that releases it only after failing on input.
/// - Trigger the failure and verify the peer receives only `err` before the connection closes.
#[ntex::test]
async fn filter_failure_discards_application_output() {
    /// Holds application output until it fails, then encodes it.
    #[derive(Debug, Default)]
    struct FailOnInput(Cell<bool>);

    impl FilterLayer for FailOnInput {
        fn process_read_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
            if buf.with_read_src(|src| src.take().is_some()) {
                self.0.set(true);
                buf.with_write_buffers(|_, dst| dst.extend_from_slice(b"err"));
                Err(io::Error::new(io::ErrorKind::InvalidData, "failed"))
            } else {
                Ok(())
            }
        }

        fn process_write_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
            if self.0.get() {
                buf.with_write_buffers(BytePages::move_to);
            }
            Ok(())
        }
    }

    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);
    let io = Io::new(server, SharedCfg::new("SRV")).add_filter(FailOnInput::default());
    io.encode_slice(b"data").unwrap();
    sleep(Millis(10)).await;

    client.write("input");
    let err = io.recv(&BytesCodec).await.unwrap_err();
    assert_eq!(err.into_inner().kind(), io::ErrorKind::InvalidData);
    sleep(Millis(50)).await;

    assert_eq!(client.read_any(), Bytes::from_static(b"err"));
    assert!(io.is_closed());
}

/// STATEMENT: Undeliverable output from a failing filter cannot hold shutdown open indefinitely.
///
/// HOW:
/// - Block peer writes and use a filter that forwards output before returning an error.
/// - Queue bytes and verify shutdown returns an error and closes within the outer timeout.
#[ntex::test]
async fn filter_failure_output_delivery_is_bounded() {
    /// Moves output to the transport, then fails.
    #[derive(Debug)]
    struct FailAfterWrite;

    impl FilterLayer for FailAfterWrite {
        fn process_read_buf(&self, _: &FilterBuf<'_>) -> io::Result<()> {
            Ok(())
        }

        fn process_write_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
            buf.with_write_buffers(BytePages::move_to);
            Err(io::Error::other("failed"))
        }
    }

    // the peer never reads
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(0);
    let io = Io::new(
        server,
        SharedCfg::new("SRV")
            .add(IoConfig::default().set_shutdown_timeout(ntex_util::time::Seconds(1))),
    )
    .add_filter(FailAfterWrite);

    let _ = io.encode_slice(b"undeliverable");
    timeout(Millis(3000), io.shutdown())
        .await
        .expect("output delivery is not bounded")
        .unwrap_err();
    assert!(io.st().flags.is_closed());
}

/// STATEMENT: A filter shutdown deadline returns `TimedOut` while closing without forceful termination.
///
/// HOW:
/// - Install a perpetually pending shutdown filter with a short deadline.
/// - Await shutdown and check the timeout error, closed state, and absence of terminating state.
#[ntex::test]
async fn filter_shutdown_timeout_is_reported() {
    #[derive(Debug)]
    struct PendingShutdown;

    impl FilterLayer for PendingShutdown {
        fn process_read_buf(&self, _: &FilterBuf<'_>) -> io::Result<()> {
            Ok(())
        }

        fn process_write_buf(&self, _: &FilterBuf<'_>) -> io::Result<()> {
            Ok(())
        }

        fn shutdown(&self, _: &FilterBuf<'_>) -> io::Result<Poll<()>> {
            Ok(Poll::Pending)
        }
    }

    let (_client, server) = IoTest::create();
    let io = Io::new(
        server,
        SharedCfg::new("SRV")
            .add(IoConfig::default().set_shutdown_timeout(ntex_util::time::Seconds(1))),
    )
    .add_filter(PendingShutdown);

    let err = timeout(Millis(3000), io.shutdown())
        .await
        .expect("transport shutdown did not complete")
        .unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    assert!(io.st().flags.is_closed());
    assert!(!io.st().flags.is_terminating());
}

/// STATEMENT: A read-blocked filter shutdown still drains its closing output through transport shutdown.
///
/// HOW:
/// - Stall peer writes, enable read pressure, and close through a filter that emits `bye` then waits.
/// - Verify transport shutdown starts, unblock writes, receive `bye`, and check the blocked-shutdown error.
#[ntex::test]
async fn blocked_filter_shutdown_flushes_buffered_output() {
    #[derive(Debug)]
    struct ClosingShutdown(Cell<bool>);

    impl FilterLayer for ClosingShutdown {
        fn process_read_buf(&self, _: &FilterBuf<'_>) -> io::Result<()> {
            Ok(())
        }

        fn process_write_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
            buf.with_write_buffers(BytePages::move_to);
            Ok(())
        }

        fn shutdown(&self, buf: &FilterBuf<'_>) -> io::Result<Poll<()>> {
            // emit a closing record, like a tls close_notify
            if !self.0.replace(true) {
                buf.with_write_buffers(|_, dst| dst.extend_from_slice(b"bye"));
            }
            Ok(Poll::Pending)
        }
    }

    let (client, server) = IoTest::create();
    // the peer cannot accept the closing record yet
    client.remote_buffer_cap(0);

    let io = Io::new(
        server,
        SharedCfg::new("SRV").add(
            IoConfig::default()
                .set_read_size(BytePageSize::Size4, BytePageSize::Size4)
                .set_read_backpressure(BytePageSize::Size4.capacity())
                .set_shutdown_timeout(ntex_util::time::Seconds(10)),
        ),
    )
    .add_filter(ClosingShutdown(Cell::new(false)));

    io.st().set_read_ready_and_backpressure();
    io.close();
    sleep(Millis(50)).await;

    // the filter shutdown is blocked, so the transport shutdown phase
    // starts right away and takes over draining the closing record
    assert!(io.st().flags.is_stopping());

    // let the peer accept the buffered bytes
    client.remote_buffer_cap(1024);
    assert_eq!(
        timeout(Millis(1000), client.read())
            .await
            .expect("closing record was not written")
            .unwrap(),
        Bytes::from_static(b"bye")
    );

    let err = timeout(Millis(1000), io.shutdown())
        .await
        .expect("transport shutdown did not complete")
        .unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::Other);
}

/// STATEMENT: Filter shutdown and transport drain share one deadline rather than separate full timeouts.
///
/// HOW:
/// - Block delivery of a closing record from a filter whose shutdown remains pending.
/// - Measure shutdown and verify closure occurs within the bound for one timeout, not two.
#[ntex::test]
async fn one_deadline_bounds_both_shutdown_phases() {
    #[derive(Debug)]
    struct ClosingShutdown(Cell<bool>);

    impl FilterLayer for ClosingShutdown {
        fn process_read_buf(&self, _: &FilterBuf<'_>) -> io::Result<()> {
            Ok(())
        }

        fn process_write_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
            buf.with_write_buffers(BytePages::move_to);
            Ok(())
        }

        fn shutdown(&self, buf: &FilterBuf<'_>) -> io::Result<Poll<()>> {
            if !self.0.replace(true) {
                buf.with_write_buffers(|_, dst| dst.extend_from_slice(b"bye"));
            }
            Ok(Poll::Pending)
        }
    }

    let (client, server) = IoTest::create();
    // the peer never accepts the closing record, so neither the filter
    // shutdown nor the transport drain can ever complete
    client.remote_buffer_cap(0);

    let io = Io::new(
        server,
        SharedCfg::new("SRV").add(
            IoConfig::default()
                .set_read_size(BytePageSize::Size4, BytePageSize::Size4)
                .set_read_backpressure(BytePageSize::Size4.capacity())
                .set_shutdown_timeout(ntex_util::time::Seconds(1)),
        ),
    )
    .add_filter(ClosingShutdown(Cell::new(false)));

    let start = std::time::Instant::now();
    io.close();

    timeout(Millis(5000), io.shutdown())
        .await
        .expect("transport shutdown did not complete")
        .unwrap_err();
    assert!(io.st().flags.is_closed());

    // both phases stall, yet a single shutdown timeout covers them: a
    // per-phase deadline would take twice as long
    let elapsed = start.elapsed();
    assert!(
        elapsed < std::time::Duration::from_millis(1600),
        "shutdown took {elapsed:?}, the deadline did not span both phases"
    );
}

/// STATEMENT: Read-blocked filter shutdown reports an error without waiting for its deadline or forcing termination.
///
/// HOW:
/// - Enable read pressure with a pending shutdown filter and a long timeout, then close.
/// - Verify prompt `Other` error delivery, closed state, and no terminating flag.
#[ntex::test]
async fn blocked_filter_shutdown_is_reported() {
    #[derive(Debug)]
    struct PendingShutdown;

    impl FilterLayer for PendingShutdown {
        fn process_read_buf(&self, _: &FilterBuf<'_>) -> io::Result<()> {
            Ok(())
        }

        fn process_write_buf(&self, _: &FilterBuf<'_>) -> io::Result<()> {
            Ok(())
        }

        fn shutdown(&self, _: &FilterBuf<'_>) -> io::Result<Poll<()>> {
            Ok(Poll::Pending)
        }
    }

    let (_client, server) = IoTest::create();
    let io = Io::new(
        server,
        SharedCfg::new("SRV").add(
            IoConfig::default()
                .set_read_size(BytePageSize::Size4, BytePageSize::Size4)
                .set_read_backpressure(BytePageSize::Size4.capacity())
                .set_shutdown_timeout(ntex_util::time::Seconds(10)),
        ),
    )
    .add_filter(PendingShutdown);

    io.st().set_read_ready_and_backpressure();
    io.close();
    sleep(Millis(50)).await;

    let err = timeout(Millis(1000), io.shutdown())
        .await
        .expect("transport shutdown did not complete")
        .unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::Other);
    assert!(io.st().flags.is_closed());
    assert!(!io.st().flags.is_terminating());
}

/// STATEMENT: Shutdown rejects new application writes and filter shutdown reports and
/// clears unprocessed output.
///
/// HOW:
/// - Start shutdown, check the write rejection, and run a layer shutdown over buffered data.
/// - Verify the shutdown fails naming the three retained bytes and the output is cleared,
///   then stop the transport and check termination errors and final closure.
#[ntex::test]
async fn shutdown() {
    // layer retains all unprocessed data at filter shutdown
    #[derive(Debug)]
    struct F;

    impl FilterLayer for F {
        fn process_read_buf(&self, _: &FilterBuf<'_>) -> io::Result<()> {
            Ok(())
        }
        fn process_write_buf(&self, _: &FilterBuf<'_>) -> io::Result<()> {
            Ok(())
        }
    }

    let io = Io::new(
        IoTest::create().0,
        SharedCfg::new("SRV").add(IoConfig::default().set_write_backpressure(8)),
    );
    let st = io.st();
    assert!(lazy(|cx| io.poll_status_update(cx)).await.is_pending());
    assert!(st.dispatch_task.is_set());
    assert!(!st.flags.is_peer_gone());
    assert!(!st.flags.is_stopping_filters());

    let ctx = IoContext::new(io.get_ref());

    // == init shutdown
    io.close();
    assert!(!st.flags.is_peer_gone());
    assert!(st.flags.is_stopping_filters());
    // encoding is not allowed in shutting down stage
    let err = io.with_write_src(|_| 1).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::Other);

    let io = io.add_filter(F);
    let layer = Layer::new(F, Base::new(io.get_ref()));

    let st = io.st();
    st.buffer
        .with_write_src(io.as_ref(), |p| p.put_slice(b"123"));
    assert_eq!(st.buffer.write_buf_size(), 3);
    let err = st
        .buffer
        .with_filter(io.as_ref(), |f| layer.shutdown(f))
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "filter layer retained 3 unwritten bytes at shutdown"
    );
    assert_eq!(st.buffer.write_buf_size(), 0);

    // == terminate
    ctx.stop(None);
    assert!(st.flags.is_peer_gone());
    assert!(st.flags.is_terminating());
    assert!(!st.flags.is_closed());
    assert!(st.flags.is_stopping_filters());

    let err = io.with_write_src(|_| 1).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotConnected);

    ctx.stopped(None);
    assert!(st.flags.is_closed());
}

struct FixedSize(usize);

impl Decoder for FixedSize {
    type Item = Bytes;
    type Error = io::Error;

    fn decode(&self, src: &mut BytesMut) -> Result<Option<Bytes>, io::Error> {
        if src.len() < self.0 {
            Ok(None)
        } else {
            Ok(Some(src.split_to(self.0)))
        }
    }
}

/// STATEMENT: Receiving reports timeout notifications even while write backpressure remains active.
///
/// HOW:
/// - Block writes, queue output above the watermark, and notify timeout while receiving.
/// - Check the timed-out transport error and verify write pressure remains set.
#[ntex::test]
async fn recv_reports_timeout_during_write_backpressure() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(0);
    let io = Io::new(
        server,
        SharedCfg::new("SRV").add(IoConfig::default().set_write_backpressure(16)),
    );
    io.encode_slice(BIN2).unwrap();
    assert!(io.flags().is_wr_backpressure());

    let ioref = io.get_ref();
    let (res, ()) =
        ntex_util::future::join(timeout(Millis(1000), io.recv(&BytesCodec)), async move {
            sleep(Millis(25)).await;
            ioref.0.notify_timeout();
        })
        .await;

    let Err(Either::Right(err)) = res.expect("recv ignored the timeout") else {
        panic!("expected a timeout error")
    };
    assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    assert!(io.flags().is_wr_backpressure());
}

/// STATEMENT: Write timeout fails send, flush, and producer readiness without closing the connection.
///
/// HOW:
/// - Stall output and await all three operations concurrently under a short write timeout.
/// - Check each returns `TimedOut`, then unblock the peer and verify a full flush succeeds.
#[ntex::test]
async fn write_timeout_bounds_output_waits() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(0);
    let io = Io::new(
        server,
        SharedCfg::new("SRV").add(
            IoConfig::default()
                .set_write_backpressure(16)
                .set_write_timeout(ntex_util::time::Seconds(1)),
        ),
    );
    io.encode_slice(BIN2).unwrap();
    assert!(io.flags().is_wr_backpressure());

    let ioref = io.get_ref();
    let ((send, flush), ready) = timeout(
        Millis(3000),
        ntex_util::future::join(
            ntex_util::future::join(
                io.send(Bytes::from_static(b"item"), &BytesCodec),
                io.flush(false),
            ),
            ioref.write_ready(),
        ),
    )
    .await
    .expect("output waits are not bounded by the write timeout");

    let Err(Either::Right(err)) = send else {
        panic!("expected a transport error")
    };
    assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    assert_eq!(flush.unwrap_err().kind(), io::ErrorKind::TimedOut);
    assert_eq!(ready.unwrap_err().kind(), io::ErrorKind::TimedOut);

    // the connection is left open, the caller decides
    assert!(!io.is_closed());
    client.remote_buffer_cap(1024);
    io.flush(true).await.unwrap();
}

/// STATEMENT: Peer EOF during an incomplete frame reports truncation without discarding buffered bytes.
///
/// HOW:
/// - Send three bytes for an eight-byte decoder and close peer input.
/// - Verify `UnexpectedEof` and the three-byte buffer remains available.
#[ntex::test]
async fn recv_reports_truncated_stream() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);
    let io = Io::new(server, SharedCfg::new("SRV"));

    // a partial item, then the peer goes away
    client.write("123");
    sleep(Millis(25)).await;
    client.close().await;

    let err = io.recv(&FixedSize(8)).await.err().unwrap();
    let Either::Right(err) = err else {
        panic!("expected a transport error")
    };
    assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);

    // the partial item is still buffered, it is not discarded
    assert_eq!(io.with_read_dst(|b| b.len()), 3);
}

/// STATEMENT: The polling receive API reports peer EOF inside a frame as `UnexpectedEof`, not as a clean `PeerGone(None)`.
///
/// HOW:
/// - Send three bytes for an eight-byte decoder and close peer input.
/// - Poll `poll_recv` to completion and verify `RecvError::PeerGone` carries `UnexpectedEof`.
/// - Start a local `close()` and verify `poll_recv_decode` on the closing path still reports `UnexpectedEof`.
#[ntex::test]
async fn poll_recv_reports_truncated_stream() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);
    let io = Io::new(server, SharedCfg::new("SRV"));

    client.write("123");
    sleep(Millis(25)).await;
    client.close().await;

    let res = poll_fn(|cx| io.poll_recv(&FixedSize(8), cx)).await;
    let Err(RecvError::PeerGone(Some(err))) = res else {
        panic!("expected truncation error")
    };
    assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    assert_eq!(io.with_read_dst(|b| b.len()), 3);

    io.close();
    sleep(Millis(25)).await;
    assert!(io.st().flags.is_stopping() || io.st().flags.is_terminating());
    let res = lazy(|cx| io.poll_recv_decode(&FixedSize(8), cx)).await;
    let Err(RecvError::PeerGone(Some(err))) = res else {
        panic!("expected truncation error while closing")
    };
    assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
}

/// STATEMENT: Clean peer EOF with an empty read buffer is reported as `PeerGone(None)`.
///
/// HOW:
/// - Send one complete four-byte frame and close peer input.
/// - Receive the frame with `poll_recv`, then verify the next poll reports `PeerGone(None)`.
#[ntex::test]
async fn poll_recv_reports_clean_eof_without_error() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);
    let io = Io::new(server, SharedCfg::new("SRV"));

    client.write("1234");
    sleep(Millis(25)).await;
    client.close().await;

    let item = poll_fn(|cx| io.poll_recv(&FixedSize(4), cx)).await;
    assert_eq!(item.ok().unwrap(), "1234");
    let res = poll_fn(|cx| io.poll_recv(&FixedSize(4), cx)).await;
    assert!(matches!(res, Err(RecvError::PeerGone(None))));
}

/// STATEMENT: A complete final frame is delivered before clean EOF.
///
/// HOW:
/// - Send one complete fixed-size frame and close peer input.
/// - Verify the first receive returns the frame and the next returns no item.
#[ntex::test]
async fn recv_reports_clean_eof() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);
    let io = Io::new(server, SharedCfg::new("SRV"));

    // a whole item, then the peer goes away
    client.write("12345678");
    sleep(Millis(25)).await;
    client.close().await;

    assert_eq!(io.recv(&FixedSize(8)).await.unwrap().unwrap(), "12345678");
    assert!(io.recv(&FixedSize(8)).await.unwrap().is_none());
}

/// Fixed size frames, the last frame may be shorter once at eof.
struct FixedSizeEof(usize, std::cell::Cell<usize>);

impl Decoder for FixedSizeEof {
    type Item = Bytes;
    type Error = io::Error;

    fn decode(&self, src: &mut BytesMut) -> Result<Option<Bytes>, io::Error> {
        FixedSize(self.0).decode(src)
    }

    fn decode_eof(&self, src: &mut BytesMut) -> Result<Option<Bytes>, io::Error> {
        self.1.set(self.1.get() + 1);
        if src.is_empty() {
            Ok(None)
        } else {
            let len = src.len().min(self.0);
            Ok(Some(src.split_to(len)))
        }
    }
}

/// STATEMENT: Receiving switches to EOF decoding only after peer EOF and can deliver a shorter final frame.
///
/// HOW:
/// - Decode a full frame while open and check no EOF-decoder calls.
/// - Send a partial frame and close, then check its delivery, EOF calls, and an empty final buffer.
#[ntex::test]
async fn recv_uses_decode_eof_after_eof() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);
    let io = Io::new(server, SharedCfg::new("SRV"));
    let codec = FixedSizeEof(8, std::cell::Cell::new(0));

    client.write("12345678");
    sleep(Millis(25)).await;
    assert_eq!(io.recv(&codec).await.unwrap().unwrap(), "12345678");
    // not at eof yet
    assert_eq!(codec.1.get(), 0);

    // a partial frame, then the peer goes away
    client.write("123");
    sleep(Millis(25)).await;
    client.close().await;

    assert_eq!(io.recv(&codec).await.unwrap().unwrap(), "123");
    assert!(io.recv(&codec).await.unwrap().is_none());
    assert!(codec.1.get() >= 2);
    assert_eq!(io.with_read_dst(|b| b.len()), 0);
}

/// STATEMENT: Local shutdown does not classify incomplete buffered input as peer truncation.
///
/// HOW:
/// - Buffer less than a full frame and initiate shutdown without peer EOF.
/// - Verify receiving returns no item successfully and preserves the partial input.
#[ntex::test]
async fn recv_local_shutdown_is_not_truncation() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);
    let io = Io::new(server, SharedCfg::new("SRV"));

    // undecodable input is left buffered, but the peer never closed and
    // the shutdown is started locally, so this is not a truncated stream
    client.write("123");
    sleep(Millis(25)).await;
    io.close();
    sleep(Millis(25)).await;

    assert!(io.recv(&FixedSize(8)).await.unwrap().is_none());
    assert_eq!(io.with_read_dst(|b| b.len()), 3);
}

/// STATEMENT: Explicit read pause stops transport input but preserves status notifications and can be cancelled.
///
/// HOW:
/// - Pause reads, send peer bytes, and verify they do not enter the application buffer.
/// - Report a timeout, then await read notification and check the pause clears and the bytes arrive.
#[ntex::test]
async fn read_pause_stops_transport_reads() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);
    let io = Io::new(server, SharedCfg::new("SRV"));

    assert!(lazy(|cx| io.poll_read_pause(cx)).await.is_pending());
    assert!(io.flags().is_read_paused());
    assert!(lazy(|cx| io.poll_read_pause(cx)).await.is_pending());

    // the transport does not read while paused
    client.write("data");
    sleep(Millis(25)).await;
    assert_eq!(io.st().buffer.read_dst_size(), 0);

    // status updates are still reported
    io.st().notify_timeout();
    assert!(matches!(
        lazy(|cx| io.poll_read_pause(cx)).await,
        Poll::Ready(IoStatusUpdate::Timeout)
    ));

    // waiting for input cancels the pause
    assert_eq!(io.read_notify().await.unwrap(), Some(()));
    assert!(!io.flags().is_read_paused());
    assert_eq!(io.with_read_dst(BytesMut::take), b"data");
}

/// STATEMENT: Querying destination-buffer size preserves read-pause and input-ready flags.
///
/// HOW:
/// - Check the empty size while paused, then receive input and check its size while marked ready.
/// - Verify both flags survive size queries and only consuming the buffer clears input readiness.
#[ntex::test]
async fn read_dst_size_keeps_read_state() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);
    let io = Io::new(server, SharedCfg::new("SRV"));

    // an installed read pause is not cancelled
    assert!(lazy(|cx| io.poll_read_pause(cx)).await.is_pending());
    assert_eq!(io.read_dst_size(), 0);
    assert!(io.flags().is_read_paused());

    // read readiness is not cleared
    client.write("data");
    assert_eq!(io.read_notify().await.unwrap(), Some(()));
    io.st().flags.set_read_ready();
    assert_eq!(io.read_dst_size(), 4);
    assert!(io.flags().is_read_ready());
    assert_eq!(io.with_read_dst(BytesMut::take), b"data");
    assert!(!io.flags().is_read_ready());
}

struct Failing;

impl Decoder for Failing {
    type Item = Bytes;
    type Error = &'static str;

    fn decode(&self, _: &mut BytesMut) -> Result<Option<Bytes>, &'static str> {
        Err("invalid frame")
    }
}

/// STATEMENT: Receiving distinguishes decoder errors from transport errors.
///
/// HOW:
/// - Send input to a decoder that returns a fixed error and verify the decoder-error variant and message.
#[ntex::test]
async fn recv_reports_decoder_error() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);
    let io = Io::new(server, SharedCfg::new("SRV"));

    client.write("data");
    let Err(Either::Left(err)) = io.recv(&Failing).await else {
        panic!("expected a decoder error")
    };
    assert_eq!(err, "invalid frame");
}

/// STATEMENT: A timeout notification takes precedence over decoding without consuming input.
///
/// HOW:
/// - Buffer a payload and notify timeout, then verify decode polling reports timeout with bytes retained.
/// - Poll again and check the decoded payload and consumed/remaining byte counts.
#[ntex::test]
async fn poll_recv_decode_reports_timeout_before_decoding() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);
    let io = Io::new(server, SharedCfg::new("SRV"));

    client.write("data");
    sleep(Millis(25)).await;
    io.st().notify_timeout();

    let res = lazy(|cx| io.poll_recv_decode(&BytesCodec, cx)).await;
    assert!(matches!(res, Err(RecvError::Timeout)));
    assert_eq!(io.st().buffer.read_dst_size(), 4);

    let decoded = lazy(|cx| io.poll_recv_decode(&BytesCodec, cx))
        .await
        .unwrap();
    assert_eq!(decoded.item.unwrap(), "data");
    assert_eq!((decoded.consumed, decoded.remains), (4, 0));
}

/// STATEMENT: Write backpressure is reported before decoding while preserving buffered input.
///
/// HOW:
/// - Buffer input, set write pressure, and verify decode polling reports pressure without consuming bytes.
/// - Clear pressure through partial flush and check the next poll returns the payload.
#[ntex::test]
async fn poll_recv_decode_reports_write_backpressure_before_decoding() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);
    let io = Io::new(server, SharedCfg::new("SRV"));

    client.write("data");
    sleep(Millis(25)).await;
    io.st().set_wr_backpressure();

    let res = lazy(|cx| io.poll_recv_decode(&BytesCodec, cx)).await;
    assert!(matches!(res, Err(RecvError::WriteBackpressure)));
    assert_eq!(io.st().buffer.read_dst_size(), 4);

    io.flush(false).await.unwrap();
    let decoded = lazy(|cx| io.poll_recv_decode(&BytesCodec, cx))
        .await
        .unwrap();
    assert_eq!(decoded.item.unwrap(), "data");
}

/// STATEMENT: Closing connections decode buffered input before disconnect, ignoring timeout and pressure notifications.
///
/// HOW:
/// - Buffer input, initiate close, and set both timeout and write-pressure notifications.
/// - Verify decode polling first returns the payload and then reports peer gone without an error.
#[ntex::test]
async fn poll_recv_decode_closing_decodes_buffered_input() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);
    let io = Io::new(server, SharedCfg::new("SRV"));

    client.write("data");
    sleep(Millis(25)).await;
    io.close();
    sleep(Millis(25)).await;
    io.st().notify_timeout();
    io.st().set_wr_backpressure();

    let decoded = lazy(|cx| io.poll_recv_decode(&BytesCodec, cx))
        .await
        .unwrap();
    assert_eq!(decoded.item.unwrap(), "data");

    let res = lazy(|cx| io.poll_recv_decode(&BytesCodec, cx)).await;
    assert!(matches!(res, Err(RecvError::PeerGone(None))));
}

/// STATEMENT: Flush polling detects write pressure for output buffered without a state update.
///
/// HOW:
/// - Append output directly while peer writes are blocked and verify pressure is initially absent.
/// - Poll partial flush to enable pressure, then drain at the peer and verify flushing clears it.
#[ntex::test]
async fn poll_flush_enables_write_backpressure() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(0);
    let io = Io::new(
        server,
        SharedCfg::new("SRV").add(IoConfig::default().set_write_backpressure(16)),
    );

    // output buffered without a state update
    io.with_write_dst(|buf| buf.extend_from_slice(BIN2));
    assert!(!io.flags().is_wr_backpressure());

    assert!(lazy(|cx| io.poll_flush(cx, false)).await.is_pending());
    assert!(io.flags().is_wr_backpressure());

    client.remote_buffer_cap(1024);
    assert_eq!(client.read().await.unwrap(), BIN2);
    io.flush(false).await.unwrap();
    assert!(!io.flags().is_wr_backpressure());
}

#[derive(Default)]
struct GateState {
    blocked: Cell<bool>,
    hold: Cell<bool>,
    fail: Cell<bool>,
    polls: Cell<usize>,
    waker: RefCell<Option<Waker>>,
}

/// Gate control, a blocked filter chain is not ready and its layers hold
/// data back.
#[derive(Clone, Default)]
struct GateCtl(Rc<GateState>);

impl fmt::Debug for GateCtl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GateCtl").finish()
    }
}

impl GateCtl {
    fn set(&self, blocked: bool) {
        self.0.blocked.set(blocked);
        self.0.hold.set(blocked);
        if let Some(waker) = self.0.waker.borrow_mut().take() {
            waker.wake();
        }
    }

    fn hold(&self) {
        self.0.hold.set(true);
    }

    fn polls(&self) -> usize {
        self.0.polls.get()
    }
}

/// Passes data through, holds input or output while the gate holds.
#[derive(Debug)]
struct GateLayer {
    ctl: GateCtl,
    input: bool,
    output: bool,
}

impl FilterLayer for GateLayer {
    fn process_read_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
        if !(self.input && self.ctl.0.hold.get()) {
            buf.with_read_buffers(|src, dst| {
                if let Some(src) = src {
                    dst.extend_from_slice(src);
                    src.clear();
                }
            });
        }
        Ok(())
    }

    fn process_write_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
        if !(self.output && self.ctl.0.hold.get()) {
            buf.with_write_buffers(BytePages::move_to);
        }
        Ok(())
    }
}

/// Filter chain wrapper that is not ready while the gate is blocked.
struct Gate<F>(F, GateCtl);

impl<F: Filter> Filter for Gate<F> {
    fn query(&self, id: std::any::TypeId) -> Option<Box<dyn std::any::Any>> {
        self.0.query(id)
    }

    fn process_read_buf(&self, ctx: &mut crate::FilterCtx<'_>) -> io::Result<()> {
        self.0.process_read_buf(ctx)
    }

    fn process_write_buf(&self, ctx: &mut crate::FilterCtx<'_>) -> io::Result<()> {
        self.0.process_write_buf(ctx)
    }

    fn shutdown(&self, ctx: &mut crate::FilterCtx<'_>) -> io::Result<Poll<()>> {
        self.0.shutdown(ctx)
    }

    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let st = &self.1.0;
        st.polls.set(st.polls.get() + 1);
        *st.waker.borrow_mut() = Some(cx.waker().clone());
        if st.fail.get() {
            Poll::Ready(Err(io::Error::other("gate")))
        } else if st.blocked.get() {
            Poll::Pending
        } else {
            self.0.poll_ready(cx)
        }
    }
}

#[derive(Debug)]
struct GateTransport;

impl IoStream for GateTransport {
    fn start(self, _: IoContext) -> Box<dyn Handle> {
        Box::new(self)
    }
}

impl Handle for GateTransport {}

fn gated<F: Filter>(io: Io<F>) -> (GateCtl, Io<Gate<F>>) {
    let gate = GateCtl::default();
    let g = gate.clone();
    (gate, io.map_filter(move |f| Gate(f, g)))
}

/// Two layers over the transport, the inner one holds output and the
/// outer one holds input in the intermediate buffers while blocked.
fn gated_layers(server: IoTest) -> (GateCtl, Io<impl Filter>) {
    let gate = GateCtl::default();
    let io = Io::from(server)
        .add_filter(GateLayer {
            ctl: gate.clone(),
            input: false,
            output: true,
        })
        .add_filter(GateLayer {
            ctl: gate.clone(),
            input: true,
            output: false,
        });
    let g = gate.clone();
    (gate, io.map_filter(move |f| Gate(f, g)))
}

/// STATEMENT: Filter readiness pauses new reads, accepts in-flight input, and notifies the dispatcher on transitions.
///
/// HOW:
/// - Block a gated filter and verify pending readiness and a single dispatcher wake while the pause persists.
/// - Complete an in-flight read, reopen the gate, and check resumed reads and a new transition notification.
#[ntex::test]
async fn filter_pause_pauses_reading() {
    let (gate, io) = gated(Io::new(GateTransport, SharedCfg::default()));
    let ctx = IoContext::new(io.get_ref());

    assert_eq!(
        lazy(|cx| ctx.poll_read_ready(cx)).await,
        Poll::Ready(Readiness::Ready)
    );
    assert!(!io.is_filter_paused());

    // the filter is not ready while the io state allows reads
    assert!(lazy(|cx| io.poll_read_more(cx)).await.is_pending());
    gate.set(true);
    assert!(lazy(|cx| ctx.poll_read_ready(cx)).await.is_pending());
    assert!(io.is_filter_paused());
    assert!(!io.st().dispatch_task.is_set(), "dispatcher is notified");

    // a continued pause does not notify again
    assert!(lazy(|cx| io.poll_read_more(cx)).await.is_pending());
    assert!(lazy(|cx| ctx.poll_read_ready(cx)).await.is_pending());
    assert!(io.st().dispatch_task.is_set());

    // a read already in flight is accepted, but reading stays paused
    let mut buf = ctx.take_read_buf();
    buf.extend_from_slice(b"12");
    assert_eq!(
        ctx.release_read_buf(buf, Poll::Ready(Ok(2))),
        IoTaskStatus::Pause
    );
    assert_eq!(io.with_read_dst(BytesMut::take), b"12");
    assert_eq!(
        ctx.with_read_buf(|_| Poll::<io::Result<usize>>::Pending),
        IoTaskStatus::Pause
    );

    // the filter is ready again
    assert!(lazy(|cx| io.poll_read_more(cx)).await.is_pending());
    gate.set(false);
    assert_eq!(
        lazy(|cx| ctx.poll_read_ready(cx)).await,
        Poll::Ready(Readiness::Ready)
    );
    assert!(!io.is_filter_paused());
    assert!(!io.st().dispatch_task.is_set(), "dispatcher is notified");
    assert_eq!(
        ctx.with_read_buf(|_| Poll::<io::Result<usize>>::Pending),
        IoTaskStatus::Io
    );
}

/// STATEMENT: Filter readiness changes are tracked independently of a dispatcher-imposed read pause.
///
/// HOW:
/// - Pause reads and toggle a gated filter from blocked to ready, checking the filter-pause flag each time.
/// - Verify reads remain pending until the dispatcher pause is removed.
#[ntex::test]
async fn filter_pause_is_tracked_while_reads_are_paused() {
    let (gate, io) = gated(Io::new(GateTransport, SharedCfg::default()));
    let ctx = IoContext::new(io.get_ref());
    gate.set(true);

    // the dispatcher paused reads, the filter pause is still observed
    io.st().flags.set_read_paused();
    assert!(lazy(|cx| ctx.poll_read_ready(cx)).await.is_pending());
    assert!(io.is_filter_paused());

    // and so is its end, reads stay paused by the io state
    gate.set(false);
    assert!(lazy(|cx| ctx.poll_read_ready(cx)).await.is_pending());
    assert!(!io.is_filter_paused());

    io.st().flags.unset_read_paused();
    assert_eq!(
        lazy(|cx| ctx.poll_read_ready(cx)).await,
        Poll::Ready(Readiness::Ready)
    );
}

/// STATEMENT: Paused filter readiness does not block transport output or require write-side readiness polling.
///
/// HOW:
/// - Block a gated filter, record its readiness-poll count, and queue transport-visible output.
/// - Verify writes are ready without another filter poll and can drain while the filter stays paused.
#[ntex::test]
async fn filter_pause_does_not_pause_writing() {
    let (gate, io) = gated(Io::new(GateTransport, SharedCfg::default()));
    let ctx = IoContext::new(io.get_ref());
    gate.set(true);
    assert!(lazy(|cx| ctx.poll_read_ready(cx)).await.is_pending());
    assert!(io.is_filter_paused());
    let polls = gate.polls();

    // output that reached the transport is written, the filter is not
    // polled for it
    io.encode_slice(b"1234").unwrap();
    sleep(Millis(5)).await;
    assert_eq!(
        lazy(|cx| ctx.poll_write_ready(cx)).await,
        Poll::Ready(Readiness::Ready)
    );
    assert_eq!(gate.polls(), polls);
    ctx.with_write_dst(|dst| drop(dst.take()));
    assert_eq!(ctx.update_write_status(Ok(4)), IoTaskStatus::Pause);
    assert_eq!(io.st().buffer.write_buf_size(), 0);
    assert!(io.is_filter_paused());
}

/// STATEMENT: Resuming filter readiness flushes held output without requiring new peer input.
///
/// HOW:
/// - Block layered filters and queue small and large payloads, checking no bytes bypass the hold.
/// - Reopen the gate, collect the expected output length, and verify its prefix and cleared filter pause.
#[ntex::test]
async fn held_output_is_written_on_resume() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1 << 20);
    let (gate, io) = gated_layers(server);

    // eager and scheduled writes do not bypass the filter, it holds the
    // output in an intermediate buffer
    gate.set(true);
    sleep(Millis(10)).await;
    assert!(io.is_filter_paused());
    io.encode_slice(b"12345678").unwrap();
    io.encode_slice(&vec![b'x'; 65536]).unwrap();
    sleep(Millis(50)).await;
    assert_eq!(client.read_any(), b"");

    // the output is written once the filter is ready, without new input
    gate.set(false);
    let buf = timeout(Millis(1000), async {
        let mut buf = BytesMut::new();
        while buf.len() < 65544 {
            buf.extend_from_slice(&client.read().await.unwrap());
        }
        buf
    })
    .await
    .unwrap();
    assert_eq!(&buf[..8], b"12345678");
    assert!(!io.is_filter_paused());
}

/// STATEMENT: Resuming filter readiness processes already-held input without another transport read.
///
/// HOW:
/// - Hold incoming bytes in intermediate filters, then pause the chain with no application input available.
/// - Reopen the gate and verify receiving delivers the original payload within the timeout.
#[ntex::test]
async fn held_input_is_processed_on_resume() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);
    let (gate, io) = gated_layers(server);

    // the input reaches the filter before it pauses, it is held in an
    // intermediate buffer
    gate.hold();
    client.write(TEXT);
    sleep(Millis(50)).await;
    assert_eq!(io.with_read_dst(|buf| buf.len()), 0);
    gate.set(true);
    sleep(Millis(10)).await;
    assert!(io.is_filter_paused());

    // the input is processed once the filter is ready, without new input
    gate.set(false);
    let item = timeout(Millis(500), io.recv(&BytesCodec)).await;
    assert_eq!(item.unwrap().unwrap().unwrap(), Bytes::from_static(BIN));
}

/// STATEMENT: Filter readiness failure requests graceful closure and preserves its error for shutdown.
///
/// HOW:
/// - Make a gated filter fail readiness and check inactive, non-terminating state and write-close readiness.
/// - Report transport stop and verify shutdown returns the gate error.
#[ntex::test]
async fn filter_readiness_error_closes() {
    let (gate, io) = gated(Io::new(GateTransport, SharedCfg::default()));
    let ctx = IoContext::new(io.get_ref());

    gate.0.fail.set(true);
    let _ = lazy(|cx| ctx.poll_read_ready(cx)).await;
    assert!(!io.is_active());
    assert!(!io.flags().is_terminating());
    assert_eq!(
        lazy(|cx| ctx.poll_write_ready(cx)).await,
        Poll::Ready(Readiness::Close)
    );
    ctx.stopped(None);
    assert_eq!(io.shutdown().await.unwrap_err().to_string(), "gate");
}

/// STATEMENT: A filter-readiness pause cannot indefinitely block connection shutdown.
///
/// HOW:
/// - Block a layered filter chain, send input, and verify the filter pause is active.
/// - Await shutdown under an outer timeout and verify it returns and closes the peer.
#[ntex::test]
async fn shutdown_does_not_wait_for_filter_readiness() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);
    let (gate, io) = gated_layers(server);
    gate.0.blocked.set(true);
    client.write(TEXT);
    sleep(Millis(50)).await;
    assert!(io.is_filter_paused());

    let res = timeout(Millis(1000), io.shutdown()).await;
    assert!(res.is_ok());
    assert!(client.is_closed());
}

#[cfg(feature = "tracker")]
mod buf_tracker {
    use ntex_bytes::BytesTracker;

    use super::*;
    use crate::BufTracker;

    fn io_with(mode: BufTracker) -> (IoTest, Io) {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        let io = Io::new(
            server,
            SharedCfg::new("SRV").add(IoConfig::default().set_buf_tracker(mode)),
        );
        (client, io)
    }

    fn cfg_with(mode: BufTracker) -> SharedCfg {
        SharedCfg::new("SRV2")
            .add(IoConfig::default().set_buf_tracker(mode))
            .into()
    }

    /// STATEMENT: Disabled buffer tracking attaches no tracker before or after receiving input.
    ///
    /// HOW:
    /// - Create an untracked connection, receive bytes, and inspect both connection and read-buffer trackers.
    #[ntex::test]
    async fn disabled() {
        let (client, io) = io_with(BufTracker::Disabled);
        assert!(io.buf_tracker().is_none());

        client.write(TEXT);
        io.read_more().await.unwrap();
        assert!(io.buf_tracker().is_none());
        assert!(io.st().get_read_buf().tracker().is_none());
    }

    /// STATEMENT: Per-connection tracking reuses one tracker per connection while isolating other connections.
    ///
    /// HOW:
    /// - Query a tracker repeatedly, receive input, and check its allocation count increases from zero to one.
    /// - Create another connection and verify it has a distinct tracker.
    #[ntex::test]
    async fn per_connection_is_created_once() {
        let (client, io) = io_with(BufTracker::PerConnection);
        let t = io.buf_tracker().unwrap();
        assert!(t.ptr_eq(io.buf_tracker().unwrap()));
        assert_eq!(t.snapshot().live_allocations, 0);

        client.write(TEXT);
        io.read_more().await.unwrap();
        assert_eq!(t.snapshot().live_allocations, 1);

        // another connection gets its own tracker
        let (_, io2) = io_with(BufTracker::PerConnection);
        assert!(!t.ptr_eq(io2.buf_tracker().unwrap()));
    }

    /// STATEMENT: Allocating a read buffer lazily creates and attaches the connection's tracker.
    ///
    /// HOW:
    /// - Obtain a read buffer before querying the connection tracker and compare their identities.
    #[ntex::test]
    async fn per_connection_created_by_read_buf() {
        let (_client, io) = io_with(BufTracker::PerConnection);
        let buf = io.st().get_read_buf();
        assert!(buf.tracker().unwrap().ptr_eq(io.buf_tracker().unwrap()));
    }

    /// STATEMENT: A shared tracker aggregates multiple connections and retains charges for received frames.
    ///
    /// HOW:
    /// - Receive input on two connections sharing one tracker and check the combined allocation count.
    /// - Retain a frame after dropping its connection, then release all owners and verify zero live charges.
    #[ntex::test]
    async fn shared() {
        let t = BytesTracker::new();
        let (client, io) = io_with(BufTracker::Shared(t.clone()));
        let (client2, io2) = io_with(BufTracker::Shared(t.clone()));
        assert!(t.ptr_eq(io.buf_tracker().unwrap()));

        client.write(TEXT);
        client2.write(TEXT);
        io.read_more().await.unwrap();
        io2.read_more().await.unwrap();
        assert_eq!(t.snapshot().live_allocations, 2);

        // the received data keeps the charge after the connection is gone
        let item = io.recv(&BytesCodec).await.unwrap().unwrap();
        drop(io);
        assert!(t.snapshot().live_allocations >= 1);
        drop(item);
        drop(io2);
        drop((client, client2));
        assert_eq!(t.snapshot().live_allocations, 0);
        assert_eq!(t.snapshot().live_bytes, 0);
    }

    /// STATEMENT: Replacing configuration with per-connection tracking preserves the existing tracker.
    ///
    /// HOW:
    /// - Capture the connection tracker, replace its configuration with the same tracking mode, and compare identity.
    #[ntex::test]
    async fn set_config_keeps_per_connection() {
        let (_client, io) = io_with(BufTracker::PerConnection);
        let t = io.buf_tracker().cloned().unwrap();
        unsafe {
            io.set_config(cfg_with(BufTracker::PerConnection));
        }
        assert!(t.ptr_eq(io.buf_tracker().unwrap()));
    }

    /// STATEMENT: Configuration changes transfer owned read-buffer charges to the replacement tracker.
    ///
    /// HOW:
    /// - Buffer input, switch from per-connection to shared tracking, and check the old charge moves.
    /// - Switch back, verify a new per-connection tracker owns the charge, and decode the unchanged payload.
    #[ntex::test]
    async fn set_config_moves_read_buffers() {
        let (client, io) = io_with(BufTracker::PerConnection);
        client.write(TEXT);
        io.read_more().await.unwrap();
        let old = io.buf_tracker().cloned().unwrap();
        assert_eq!(old.snapshot().live_allocations, 1);

        let t = BytesTracker::new();
        unsafe {
            io.set_config(cfg_with(BufTracker::Shared(t.clone())));
        }
        assert!(t.ptr_eq(io.buf_tracker().unwrap()));
        assert_eq!(old.snapshot().live_allocations, 0);
        assert_eq!(t.snapshot().live_allocations, 1);

        // switching back creates a new per-connection tracker
        unsafe {
            io.set_config(cfg_with(BufTracker::PerConnection));
        }
        let t2 = io.buf_tracker().cloned().unwrap();
        assert!(!t2.ptr_eq(&old));
        assert_eq!(t.snapshot().live_allocations, 0);
        assert_eq!(t2.snapshot().live_allocations, 1);

        let item = io.recv(&BytesCodec).await.unwrap().unwrap();
        assert_eq!(item, TEXT.as_bytes());
    }

    /// STATEMENT: Disabling tracking leaves existing buffers charged but does not track new read buffers.
    ///
    /// HOW:
    /// - Receive tracked input, disable tracking, and inspect the connection and a newly obtained buffer.
    /// - Check the old charge persists until the buffered frame and connection are released.
    #[ntex::test]
    async fn set_config_disabled() {
        let t = BytesTracker::new();
        let (client, io) = io_with(BufTracker::Shared(t.clone()));
        client.write(TEXT);
        io.read_more().await.unwrap();

        unsafe {
            io.set_config(cfg_with(BufTracker::Disabled));
        }
        assert!(io.buf_tracker().is_none());
        assert!(io.st().get_read_buf().tracker().is_none());
        assert_eq!(t.snapshot().live_allocations, 1);

        drop(io.recv(&BytesCodec).await.unwrap().unwrap());
        drop(io);
        assert_eq!(t.snapshot().live_allocations, 0);
    }

    const DATA: [u8; 1000] = [b'a'; 1000];

    /// STATEMENT: A retained frame keeps its allocation charged without retaining connection state.
    ///
    /// HOW:
    /// - Receive a frame, drop its connection, and close the peer while holding a weak state reference.
    /// - Verify state destruction with a live charge, then drop the frame and check all charges clear.
    #[ntex::test]
    async fn frame_outlives_io() {
        let (client, io) = io_with(BufTracker::PerConnection);
        let t = io.buf_tracker().cloned().unwrap();
        let state = Rc::downgrade(&io.get_ref().0);

        client.write(DATA);
        let item = io.recv(&BytesCodec).await.unwrap().unwrap();
        assert_eq!(item.len(), DATA.len());

        drop(io);
        client.close().await;
        sleep(Millis(50)).await;
        assert!(state.upgrade().is_none());
        assert_eq!(t.snapshot().live_allocations, 1);
        assert!(t.snapshot().live_bytes >= DATA.len());

        drop(item);
        assert_eq!(t.snapshot().live_allocations, 0);
        assert_eq!(t.snapshot().live_bytes, 0);
    }

    /// STATEMENT: Releasing a received frame on another thread clears its shared tracker charges.
    ///
    /// HOW:
    /// - Receive a frame and drop the connection and peer while retaining the frame's allocation.
    /// - Drop the frame in a joined thread and verify zero live allocations and bytes.
    #[ntex::test]
    async fn release_on_other_thread() {
        let t = BytesTracker::new();
        let (client, io) = io_with(BufTracker::Shared(t.clone()));
        client.write(DATA);
        let item = io.recv(&BytesCodec).await.unwrap().unwrap();
        drop(io);
        drop(client);
        assert_eq!(t.snapshot().live_allocations, 1);

        std::thread::spawn(move || drop(item)).join().unwrap();
        assert_eq!(t.snapshot().live_allocations, 0);
        assert_eq!(t.snapshot().live_bytes, 0);
    }

    /// STATEMENT: In-flight read buffers are charged until empty release or final decoded-frame destruction.
    ///
    /// HOW:
    /// - Take a tracked buffer, release it pending and empty, and check the allocation charge clears.
    /// - Complete another buffer with data, decode it, and verify its charge outlives the connection until frame drop.
    #[ntex::test]
    async fn pending_read() {
        let io = Io::new(ManualTransport, cfg_with(BufTracker::PerConnection));
        let ctx = IoContext::new(io.get_ref());
        let t = io.buf_tracker().cloned().unwrap();

        let buf = ctx.take_read_buf();
        assert!(buf.tracker().unwrap().ptr_eq(&t));
        assert_eq!(t.snapshot().live_allocations, 1);
        let _ = ctx.release_read_buf(buf, Poll::Pending);
        assert_eq!(t.snapshot().live_allocations, 0);

        let mut buf = ctx.take_read_buf();
        assert_eq!(t.snapshot().live_allocations, 1);
        buf.extend_from_slice(&DATA);
        let _ = ctx.release_read_buf(buf, Poll::Ready(Ok(DATA.len())));
        let item = io.recv(&BytesCodec).await.unwrap().unwrap();
        assert_eq!(item.len(), DATA.len());

        drop((io, ctx));
        assert_eq!(t.snapshot().live_allocations, 1);
        drop(item);
        assert_eq!(t.snapshot().live_allocations, 0);
    }

    /// STATEMENT: A read completion arriving after connection drop discards its buffer and releases its charge.
    ///
    /// HOW:
    /// - Take a tracked read buffer, drop `Io`, and verify the in-flight allocation remains charged.
    /// - Complete it with data and check live allocation and byte counts become zero.
    #[ntex::test]
    async fn cancelled_read() {
        let io = Io::new(ManualTransport, cfg_with(BufTracker::PerConnection));
        let ctx = IoContext::new(io.get_ref());
        let t = io.buf_tracker().cloned().unwrap();

        let mut buf = ctx.take_read_buf();
        drop(io);
        assert_eq!(t.snapshot().live_allocations, 1);

        // the input is discarded
        buf.extend_from_slice(&DATA);
        let _ = ctx.release_read_buf(buf, Poll::Ready(Ok(DATA.len())));
        assert_eq!(t.snapshot().live_allocations, 0);
        assert_eq!(t.snapshot().live_bytes, 0);
        drop(ctx);
    }

    #[derive(Debug)]
    struct CopyLayer;

    impl FilterLayer for CopyLayer {
        fn process_read_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
            buf.with_read_buffers(|src, dst| {
                if let Some(src) = src {
                    dst.extend_from_slice(src);
                    src.clear();
                }
            });
            Ok(())
        }

        fn process_write_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
            buf.with_write_buffers(BytePages::move_to);
            Ok(())
        }
    }

    /// STATEMENT: A filter's decoded destination allocation remains tracked while its output frame is retained.
    ///
    /// HOW:
    /// - Complete a read through a copying filter and receive its output frame.
    /// - Drop connection state, check the frame's allocation and byte charges, then drop it and verify release.
    #[ntex::test]
    async fn filter_layer_destination() {
        let t = BytesTracker::new();
        let io =
            Io::new(ManualTransport, cfg_with(BufTracker::Shared(t.clone()))).add_filter(CopyLayer);
        let ctx = IoContext::new(io.get_ref());

        let _ = ctx.with_read_buf(|buf| {
            buf.extend_from_slice(&DATA);
            Poll::Ready(Ok(DATA.len()))
        });
        let item = io.recv(&BytesCodec).await.unwrap().unwrap();
        assert_eq!(item.len(), DATA.len());

        // the transport buffer goes with the connection, the output stays
        drop((io, ctx));
        assert_eq!(t.snapshot().live_allocations, 1);
        assert!(t.snapshot().live_bytes >= DATA.len());
        drop(item);
        assert_eq!(t.snapshot().live_allocations, 0);
    }
}
