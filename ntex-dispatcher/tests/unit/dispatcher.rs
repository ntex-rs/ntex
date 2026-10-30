use std::sync::{Arc, Mutex, atomic::AtomicBool, atomic::Ordering::Relaxed};
use std::{cell::RefCell, io};

use ntex_bytes::{BytePageSize, BytePages, Bytes, BytesMut};
use ntex_codec::BytesCodec;
use ntex_io::{Io, IoConfig, IoRef, testing::IoTest};
use ntex_service::{BindCtx, Ctx, Pipeline, ReadyNotifier, Service, cfg::SharedCfg};
use ntex_util::time::{Millis, sleep, timeout};
use ntex_util::{channel::condition::Condition, channel::oneshot, future::lazy, spawn};
use rand::Rng;

use super::*;

/// Waits until `f` returns `true`, up to `max`.
async fn wait_until(max: Millis, f: impl Fn() -> bool) {
    for _ in 0..max.0 / 50 {
        if f() {
            break;
        }
        sleep(Millis(50)).await;
    }
}

/// Waits until the peer is closed, up to `max`.
async fn wait_closed(client: &IoTest, max: Millis) {
    wait_until(max, || client.is_closed()).await;
}

pub(crate) struct State(IoRef);

impl State {
    fn io(&self) -> &IoRef {
        &self.0
    }

    fn close(&self) {
        self.0.close();
    }
}

#[derive(Copy, Clone)]
struct BCodec(usize);

impl Encoder for BCodec {
    type Item = Bytes;
    type Error = io::Error;

    fn encode(&self, item: Bytes, dst: &mut BytePages) -> Result<(), Self::Error> {
        dst.append(item);
        Ok(())
    }
}

impl Decoder for BCodec {
    type Item = Bytes;
    type Error = io::Error;

    fn decode(&self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if src.len() < self.0 {
            Ok(None)
        } else {
            Ok(Some(src.split_to(self.0)))
        }
    }
}

impl<U, Err> Dispatcher<U, Err>
where
    U: Decoder + Encoder + 'static,
{
    /// Construct new `Dispatcher` instance
    pub(crate) fn debug<S>(io: Io, codec: U, service: S) -> (Self, State)
    where
        S: Service<(), DispatchItem<U>, Res = Option<Response<U>>, Error = Err> + 'static,
    {
        let st = State(io.get_ref());
        (Dispatcher::new(io, codec, Pipeline::new((), service)), st)
    }
}

/// STATEMENT: The dispatcher sends service responses and releases the transport after peer closure.
///
/// HOW:
/// - Send two requests through an `IoTest` connection with a delayed echo service.
/// - Compare both responses with the requests, then close the peer and verify the server is dropped.
#[ntex::test]
async fn basics() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);
    client.write("GET /test HTTP/1\r\n\r\n");

    let (disp, _) = Dispatcher::debug(
        Io::from(server),
        BytesCodec,
        ntex_service::fn_service(|msg: DispatchItem<BytesCodec>| async move {
            sleep(Millis(50)).await;
            if let DispatchItem::Item(msg) = msg {
                Ok::<_, ()>(Some(msg))
            } else {
                Ok(None)
            }
        }),
    );
    spawn(async move {
        let _ = disp.await;
    });

    sleep(Millis(25)).await;
    let buf = client.read().await.unwrap();
    assert_eq!(buf, Bytes::from_static(b"GET /test HTTP/1\r\n\r\n"));

    client.write("GET /test HTTP/1\r\n\r\n");
    let buf = client.read().await.unwrap();
    assert_eq!(buf, Bytes::from_static(b"GET /test HTTP/1\r\n\r\n"));

    client.close().await;
    sleep(Millis(75)).await;
    assert!(client.is_server_dropped());
}

/// STATEMENT: Directly encoded output shares the dispatcher transport, which application closure releases.
///
/// HOW:
/// - Verify an echoed request, then send additional bytes through `IoRef::encode`.
/// - Check the additional response and close the dispatcher, observing that the server is dropped.
#[ntex::test]
async fn sink() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);
    client.write("GET /test HTTP/1\r\n\r\n");

    let (disp, st) = Dispatcher::debug(
        Io::from(server),
        BytesCodec,
        ntex_service::fn_service(|msg: DispatchItem<BytesCodec>| async move {
            if let DispatchItem::Item(msg) = msg {
                Ok::<_, ()>(Some(msg))
            } else if let DispatchItem::Stop(_) = msg {
                Ok(None)
            } else {
                panic!()
            }
        }),
    );
    spawn(async move {
        let _ = disp.await;
    });

    let buf = client.read().await.unwrap();
    assert_eq!(buf, Bytes::from_static(b"GET /test HTTP/1\r\n\r\n"));

    assert!(
        st.io()
            .encode(Bytes::from_static(b"test"), &BytesCodec)
            .is_ok()
    );
    let buf = client.read().await.unwrap();
    assert_eq!(buf, Bytes::from_static(b"test"));

    st.close();
    wait_until(Millis(2000), || client.is_server_dropped()).await;
    assert!(client.is_server_dropped());
}

/// STATEMENT: A service call failure flushes buffered output before closing the transport.
///
/// HOW:
/// - Block writes, queue output, and dispatch a request to a service that returns an error.
/// - Allow writes and verify the queued bytes arrive, the peer closes, and peer shutdown drops the server.
#[ntex::test]
async fn err_in_service() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(0);
    client.write("GET /test HTTP/1\r\n\r\n");

    let (disp, state) = Dispatcher::debug(
        Io::new(server, SharedCfg::new("SRV")),
        BytesCodec,
        ntex_service::fn_service(|_: DispatchItem<BytesCodec>| async move {
            Err::<Option<Bytes>, _>(())
        }),
    );
    state
        .io()
        .encode(Bytes::from_static(b"GET /test HTTP/1\r\n\r\n"), &BytesCodec)
        .unwrap();
    spawn(async move {
        let _ = disp.await;
    });

    // buffer should be flushed
    client.remote_buffer_cap(1024);
    let buf = client.read().await.unwrap();
    assert_eq!(buf, Bytes::from_static(b"GET /test HTTP/1\r\n\r\n"));

    // write side must be closed, dispatcher waiting for read side to close
    wait_closed(&client, Millis(1000)).await;
    assert!(client.is_closed());

    // close read side
    client.close().await;

    // dispatcher is closed
    assert!(client.is_server_dropped());
}

/// STATEMENT: A readiness failure is returned while buffered output and transport shutdown are completed.
///
/// HOW:
/// - Count failing readiness checks and queue output while socket writes are blocked.
/// - Release writes and check the response, returned readiness error, and transport closure.
/// - Verify the shutdown path performs four readiness checks in total.
#[ntex::test]
#[allow(clippy::items_after_statements)]
async fn err_in_service_ready() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(0);
    client.write("GET /test HTTP/1\r\n\r\n");

    let counter = Rc::new(Cell::new(0));

    struct Srv(Rc<Cell<usize>>);

    impl Service<(), DispatchItem<BytesCodec>> for Srv {
        type Res = Option<Response<BytesCodec>>;
        type Error = &'static str;

        async fn ready(&self, _: Ctx<'_, Self>) -> Result<(), Self::Error> {
            self.0.set(self.0.get() + 1);
            Err("test")
        }

        async fn call(
            &self,
            _: DispatchItem<BytesCodec>,
            _: Ctx<'_, Self>,
        ) -> Result<Self::Res, Self::Error> {
            Ok(None)
        }
    }

    let (disp, state) = Dispatcher::debug(Io::from(server), BytesCodec, Srv(counter.clone()));
    spawn(async move {
        let res = disp.await;
        assert_eq!(res, Err("test"));
    });

    state
        .io()
        .encode(Bytes::from_static(b"GET /test HTTP/1\r\n\r\n"), &BytesCodec)
        .unwrap();

    // buffer should be flushed
    client.remote_buffer_cap(1024);
    let buf = client.read().await.unwrap();
    assert_eq!(buf, Bytes::from_static(b"GET /test HTTP/1\r\n\r\n"));

    // write side must be closed, dispatcher waiting for read side to close
    wait_closed(&client, Millis(1000)).await;
    assert!(client.is_closed());

    // close read side
    client.close().await;
    assert!(client.is_server_dropped());

    // service must be checked for readiness all the time,
    // the stop call checks readiness as well
    assert_eq!(counter.get(), 4);
}

/// STATEMENT: Buffered output drives write-backpressure notifications as the peer drains data.
///
/// HOW:
/// - Block socket writes and produce a 65,536-byte response above the configured write watermark.
/// - Send another request and release output capacity in stages, checking input consumption and buffered sizes.
/// - Verify the service observes one item followed by backpressure-enabled and backpressure-disabled events.
#[ntex::test]
async fn write_backpressure() {
    let (client, server) = IoTest::create();
    // do not allow to write to socket
    client.remote_buffer_cap(0);
    client.write("GET /test HTTP/1\r\n\r\n");

    let data = Arc::new(Mutex::new(RefCell::new(Vec::new())));
    let data2 = data.clone();

    let io = Io::new(
        server,
        SharedCfg::new("TEST").add(
            IoConfig::new()
                .set_read_size(BytePageSize::Size8, BytePageSize::Size8)
                .set_read_backpressure(BytePageSize::Size8.capacity())
                .set_write_backpressure(16 * 1024),
        ),
    );

    let (disp, state) = Dispatcher::debug(
        io,
        BytesCodec,
        ntex_service::fn_service(move |msg: DispatchItem<BytesCodec>| {
            let data = data2.clone();
            async move {
                match msg {
                    DispatchItem::Item(_) => {
                        data.lock().unwrap().borrow_mut().push(0);
                        let bytes = rand::rng()
                            .sample_iter(&rand::distr::Alphanumeric)
                            .take(65_536)
                            .map(char::from)
                            .collect::<String>();
                        return Ok::<_, ()>(Some(Bytes::from(bytes)));
                    }
                    DispatchItem::Control(Control::WBackPressureEnabled) => {
                        data.lock().unwrap().borrow_mut().push(1);
                    }
                    DispatchItem::Control(Control::WBackPressureDisabled) => {
                        data.lock().unwrap().borrow_mut().push(2);
                    }
                    _ => (),
                }
                Ok(None)
            }
        }),
    );

    spawn(async move {
        let _ = disp.await;
    });

    let buf = client.read_any();
    assert_eq!(buf, Bytes::from_static(b""));
    client.write("GET /test HTTP/1\r\n\r\n");
    sleep(Millis(25)).await;

    // buf must be consumed
    assert_eq!(client.remote_buffer(|buf| buf.len()), 0);

    // response message
    assert_eq!(state.io().with_write_src(|buf| buf.len()).unwrap(), 65536);

    client.remote_buffer_cap(10240);
    sleep(Millis(50)).await;
    assert_eq!(state.io().with_write_src(|buf| buf.len()).unwrap(), 55296);

    client.remote_buffer_cap(48056);
    sleep(Millis(50)).await;
    assert_eq!(state.io().with_write_src(|buf| buf.len()).unwrap(), 7240);

    // backpressure disabled
    assert_eq!(&data.lock().unwrap().borrow()[..], &[0, 1, 2]);
}

/// STATEMENT: Closing the dispatcher during read backpressure allows its dispatch loop to finish.
///
/// HOW:
/// - Limit the service to one slow call and send two buffers sized to fill the read watermark.
/// - Close the dispatcher and await the completion channel sent when its future returns.
#[ntex::test]
async fn disconnect_during_read_backpressure() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(0);

    let (disp, state) = Dispatcher::debug(
        Io::new(
            server,
            SharedCfg::new("TEST").add(
                IoConfig::new()
                    .set_keepalive_timeout(Seconds::ZERO)
                    .set_read_size(BytePageSize::Size4, BytePageSize::Size4)
                    .set_read_backpressure(BytePageSize::Size4.capacity()),
            ),
        ),
        BytesCodec,
        ntex_util::services::inflight::InFlightService::new(
            1,
            ntex_service::fn_service(move |msg: DispatchItem<BytesCodec>| async move {
                if let DispatchItem::Item(_) = msg {
                    sleep(Millis(500)).await;
                    Ok::<_, ()>(None)
                } else {
                    Ok(None)
                }
            }),
        ),
    );

    let (tx, rx) = ntex::channel::oneshot::channel();
    ntex::rt::spawn(async move {
        let _ = disp.await;
        let _ = tx.send(());
    });

    let bytes = rand::rng()
        .sample_iter(&rand::distr::Alphanumeric)
        .take(BytePageSize::Size4.capacity())
        .map(char::from)
        .collect::<String>();
    client.write(bytes.clone());
    sleep(Millis(25)).await;
    client.write(bytes);
    sleep(Millis(25)).await;

    // close read side
    state.close();
    let _ = rx.recv().await;
}

/// STATEMENT: Application closure waits for an active service call to finish.
///
/// HOW:
/// - Poll until a request handler starts and blocks on a condition, then close the dispatcher.
/// - Verify dispatcher completion times out while the call is blocked and succeeds after releasing it.
#[ntex::test]
async fn application_close_waits_for_pending_service_call() {
    let started = Arc::new(AtomicBool::new(false));
    let started2 = started.clone();
    let release = Condition::<()>::new();
    let release2 = release.clone();
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);

    let (mut disp, state) = Dispatcher::debug(
        Io::from(server),
        BytesCodec,
        ntex_service::fn_service(move |msg: DispatchItem<BytesCodec>| {
            let started = started2.clone();
            let waiter = release2.wait();
            async move {
                if matches!(msg, DispatchItem::Item(_)) {
                    started.store(true, Relaxed);
                    let _ = waiter.ready().await;
                }
                Ok::<_, ()>(None)
            }
        }),
    );

    client.write("request");
    for _ in 0..100 {
        assert!(lazy(|cx| Pin::new(&mut disp).poll(cx)).await.is_pending());
        if started.load(Relaxed) {
            break;
        }
        sleep(Millis(10)).await;
    }
    assert!(started.load(Relaxed));

    // the service call is pending until it is released
    state.close();
    assert!(
        timeout(Millis(50), poll_fn(|cx| Pin::new(&mut disp).poll(cx)))
            .await
            .is_err()
    );
    release.notify(());
    timeout(Millis(1000), poll_fn(|cx| Pin::new(&mut disp).poll(cx)))
        .await
        .expect("dispatcher did not drain pending service call")
        .unwrap();
}

/// STATEMENT: Dropping a service call releases its in-flight slot and wakes the dispatcher.
///
/// HOW:
/// - Create both unpolled and pending-polled calls against a service that never completes.
/// - Register a dispatcher waker, drop each call, and verify the count falls from one to zero and wakes it.
#[ntex::test]
async fn cancelled_calls_release_inflight() {
    struct Wake(AtomicBool);

    impl std::task::Wake for Wake {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Relaxed);
        }
    }

    let (_client, server) = IoTest::create();
    let disp = Dispatcher::new(
        Io::from(server),
        BytesCodec,
        Pipeline::new(
            (),
            ntex_service::fn_service(|_: DispatchItem<BytesCodec>| {
                std::future::pending::<Result<Option<Bytes>, ()>>()
            }),
        ),
    );
    let shared = &disp.inner.shared;
    let wake = Arc::new(Wake(AtomicBool::new(false)));
    let waker = std::task::Waker::from(wake.clone());
    let mut cx = Context::from_waker(&waker);

    for polled in [false, true] {
        let mut call = shared.call(DispatchItem::Item(Bytes::from_static(b"request")));
        if polled {
            assert!(Pin::new(&mut call).poll(&mut cx).is_pending());
        }
        assert_eq!(shared.inflight.get(), 1);
        shared.io.register_dispatch(&mut cx);
        wake.0.store(false, Relaxed);
        drop(call);
        assert_eq!(shared.inflight.get(), 0);
        assert!(wake.0.load(Relaxed), "cancellation must wake shutdown");
    }
}

/// STATEMENT: Stop handoff cancels the owned response while detached requests drain before service shutdown.
///
/// HOW:
/// - Start one or two blocked calls, block readiness, and terminate the transport with a shutdown deadline.
/// - Keep the completed dispatcher alive and verify its response is cleared and one request is dropped.
/// - Restore readiness, observe one stop call, and release the detached request when present.
/// - Check shutdown runs once, all in-flight calls drain, and dropping the dispatcher releases the service.
#[ntex::test]
async fn stop_handoff_cancels_owned_response() {
    #[derive(Default)]
    struct Data {
        blocked: Cell<bool>,
        dropped: Cell<usize>,
        stopped: Cell<usize>,
        shutdown: Cell<usize>,
        ready: Condition,
        requests: Condition,
        changed: ReadyNotifier,
    }

    struct Request(Rc<Data>);

    impl Drop for Request {
        fn drop(&mut self) {
            self.0.dropped.set(self.0.dropped.get() + 1);
        }
    }

    struct Srv {
        data: Rc<Data>,
        _token: Rc<()>,
    }

    impl Service<(), DispatchItem<BCodec>> for Srv {
        type Res = Option<Bytes>;
        type Error = ();

        fn bind(&self, ctx: BindCtx<'_>) {
            ctx.subscribe(&self.data.changed);
        }

        async fn ready(&self, _: Ctx<'_, Self>) -> Result<(), ()> {
            while self.data.blocked.get() {
                self.data.ready.wait().await;
            }
            Ok(())
        }

        async fn call(
            &self,
            item: DispatchItem<BCodec>,
            _: Ctx<'_, Self>,
        ) -> Result<Self::Res, ()> {
            match item {
                DispatchItem::Item(_) => {
                    let _request = Request(self.data.clone());
                    self.data.requests.wait().await;
                }
                DispatchItem::Stop(_) => {
                    self.data.stopped.set(self.data.stopped.get() + 1);
                }
                DispatchItem::Control(_) => (),
            }
            Ok(None)
        }

        async fn shutdown(&self, _: Ctx<'_, Self>) {
            self.data.shutdown.set(self.data.shutdown.get() + 1);
        }
    }

    for count in [1, 2] {
        let (_client, server) = IoTest::create();
        let io = Io::new(
            server,
            SharedCfg::new("TEST").add(
                IoConfig::new()
                    .set_keepalive_timeout(Seconds::ZERO)
                    .set_shutdown_timeout(Seconds(1)),
            ),
        );
        io.with_read_dst(|buf| buf.extend_from_slice(&b"xx"[..count]));
        let ioref = io.get_ref();
        let data = Rc::new(Data::default());
        let token = Rc::new(());
        let weak = Rc::downgrade(&token);
        let mut disp = Dispatcher::new(
            io,
            BCodec(1),
            Pipeline::new(
                (),
                Srv {
                    data: data.clone(),
                    _token: token,
                },
            ),
        );
        assert!(lazy(|cx| Pin::new(&mut disp).poll(cx)).await.is_pending());
        assert_eq!(disp.inner.shared.inflight.get(), count as u32);
        data.blocked.set(true);
        data.changed.notify();
        ioref.terminate();
        timeout(Millis(2500), &mut disp).await.unwrap().unwrap();

        // Keep the completed dispatcher alive: handoff must cancel the response.
        assert!(disp.inner.response.is_none());
        assert_eq!(data.dropped.get(), 1);
        assert_eq!(disp.inner.shared.inflight.get(), count as u32);
        data.blocked.set(false);
        data.ready.notify(());
        wait_until(Millis(1000), || data.stopped.get() == 1).await;
        assert_eq!(data.stopped.get(), 1);

        if count == 2 {
            assert_eq!(data.shutdown.get(), 0);
            // The separately detached request still drains and wakes shutdown.
            data.requests.notify(());
        }
        wait_until(Millis(1000), || data.shutdown.get() == 1).await;
        assert_eq!(data.shutdown.get(), 1);
        assert_eq!(disp.inner.shared.inflight.get(), 0);
        assert_eq!(data.dropped.get(), count);
        drop(disp);
        wait_until(Millis(1000), || weak.upgrade().is_none()).await;
        assert!(weak.upgrade().is_none());
    }
}

/// STATEMENT: The dispatcher defers requests beyond its configured in-flight limit.
///
/// HOW:
/// - Set a limit of two, send three requests, and block their handlers on a condition.
/// - Verify only two calls start before release and the third starts after notifying the condition.
#[ntex::test]
async fn max_inflight_calls() {
    let calls = Rc::new(Cell::new(0));
    let calls2 = calls.clone();
    let release = Condition::<()>::new();
    let release2 = release.clone();
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);

    let (disp, _) = Dispatcher::debug(
        Io::from(server),
        BytesCodec,
        ntex_service::fn_service(move |msg: DispatchItem<BytesCodec>| {
            let waiter = release2.wait();
            if matches!(msg, DispatchItem::Item(_)) {
                calls2.set(calls2.get() + 1);
            }
            async move {
                let _ = waiter.ready().await;
                Ok::<_, ()>(None)
            }
        }),
    );
    ntex::rt::spawn(disp.max_inflight(2));

    for msg in ["a", "b", "c"] {
        client.write(msg);
        sleep(Millis(25)).await;
    }
    assert_eq!(calls.get(), 2);

    release.notify(());
    sleep(Millis(25)).await;
    assert_eq!(calls.get(), 3);
}

/// STATEMENT: An idle connection closes with a keep-alive stop notification and close reason.
///
/// HOW:
/// - Echo one request with a one-second keep-alive timeout, then leave the connection idle.
/// - Verify the transport becomes inactive, the peer closes, and the item is followed by a keep-alive stop.
/// - Check the stored close reason is `CloseReason::KeepAlive`.
#[ntex::test]
async fn keepalive() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);
    client.write("GET /test HTTP/1\r\n\r\n");

    let data = Arc::new(Mutex::new(RefCell::new(Vec::new())));
    let data2 = data.clone();

    let cfg = SharedCfg::new("DBG").add(
        IoConfig::new()
            .set_shutdown_timeout(Seconds(1))
            .set_keepalive_timeout(Seconds(1)),
    );

    let (disp, state) = Dispatcher::debug(
        Io::new(server, cfg),
        BytesCodec,
        ntex_service::fn_service(move |msg: DispatchItem<BytesCodec>| {
            let data = data2.clone();
            async move {
                match msg {
                    DispatchItem::Item(bytes) => {
                        data.lock().unwrap().borrow_mut().push(0);
                        return Ok::<_, ()>(Some(bytes));
                    }
                    DispatchItem::Stop(Reason::KeepAlive) => {
                        data.lock().unwrap().borrow_mut().push(1);
                    }
                    _ => (),
                }
                Ok(None)
            }
        }),
    );
    spawn(async move {
        let _ = disp.await;
    });

    let buf = client.read().await.unwrap();
    assert_eq!(buf, Bytes::from_static(b"GET /test HTTP/1\r\n\r\n"));
    wait_until(Millis(3000), || {
        !state.0.is_active() && client.is_closed() && data.lock().unwrap().borrow().len() == 2
    })
    .await;

    // write side must be closed, dispatcher should fail with keep-alive
    assert!(!state.0.is_active());
    assert!(client.is_closed());
    assert_eq!(&data.lock().unwrap().borrow()[..], &[0, 1]);
    assert_eq!(state.0.close_reason(), Some(CloseReason::KeepAlive));
}

/// STATEMENT: Keep-alive expiry still closes an idle connection when frame-read tracking is configured.
///
/// HOW:
/// - Configure both timers and echo a complete eight-byte frame.
/// - Wait for inactivity and verify peer closure and an item followed by a keep-alive stop notification.
#[ntex::test]
async fn keepalive2() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);

    let data = Arc::new(Mutex::new(RefCell::new(Vec::new())));
    let data2 = data.clone();

    let cfg = SharedCfg::new("DBG").add(
        IoConfig::new()
            .set_keepalive_timeout(Seconds(1))
            .set_frame_read_rate(Seconds(1), Seconds(2), 2),
    );

    let (disp, state) = Dispatcher::debug(
        Io::new(server, cfg),
        BCodec(8),
        ntex_service::fn_service(move |msg: DispatchItem<BCodec>| {
            let data = data2.clone();
            async move {
                match msg {
                    DispatchItem::Item(bytes) => {
                        data.lock().unwrap().borrow_mut().push(0);
                        return Ok::<_, ()>(Some(bytes));
                    }
                    DispatchItem::Stop(Reason::KeepAlive) => {
                        data.lock().unwrap().borrow_mut().push(1);
                    }
                    _ => (),
                }
                Ok(None)
            }
        }),
    );
    spawn(async move {
        let _ = disp.await;
    });

    client.write("12345678");
    let buf = client.read().await.unwrap();
    assert_eq!(buf, Bytes::from_static(b"12345678"));
    wait_until(Millis(3000), || {
        !state.0.is_active() && client.is_closed() && data.lock().unwrap().borrow().len() == 2
    })
    .await;

    // write side must be closed, dispatcher should fail with keep-alive
    assert!(!state.0.is_active());
    assert!(client.is_closed());
    assert_eq!(&data.lock().unwrap().borrow()[..], &[0, 1]);
}

/// STATEMENT: Receiving complete frames refreshes the connection's keep-alive deadline.
///
/// HOW:
/// - Send three one-byte frames at half-second intervals with a one-second keep-alive timeout.
/// - Verify every echo and check the connection remains open with only the three item events recorded.
#[ntex::test]
async fn keepalive3() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);

    let data = Arc::new(Mutex::new(RefCell::new(Vec::new())));
    let data2 = data.clone();

    let cfg = SharedCfg::new("DBG").add(
        IoConfig::new()
            .set_keepalive_timeout(Seconds(1))
            .set_frame_read_rate(Seconds(1), Seconds(2), 2),
    );

    let (disp, _) = Dispatcher::debug(
        Io::new(server, cfg),
        BCodec(1),
        ntex_service::fn_service(move |msg: DispatchItem<BCodec>| {
            let data = data2.clone();
            async move {
                match msg {
                    DispatchItem::Item(bytes) => {
                        data.lock().unwrap().borrow_mut().push(0);
                        return Ok::<_, ()>(Some(bytes));
                    }
                    DispatchItem::Stop(Reason::KeepAlive) => {
                        data.lock().unwrap().borrow_mut().push(1);
                    }
                    _ => (),
                }
                Ok(None)
            }
        }),
    );
    spawn(async move {
        let _ = disp.await;
    });

    client.write("1");
    let buf = client.read().await.unwrap();
    assert_eq!(buf, Bytes::from_static(b"1"));
    sleep(Millis(500)).await;

    client.write("2");
    let buf = client.read().await.unwrap();
    assert_eq!(buf, Bytes::from_static(b"2"));

    sleep(Millis(500)).await;
    client.write("3");
    let buf = client.read().await.unwrap();
    assert_eq!(buf, Bytes::from_static(b"3"));

    sleep(Millis(500)).await;
    assert!(!client.is_closed());
    assert_eq!(&data.lock().unwrap().borrow()[..], &[0, 0, 0]);
}

/// STATEMENT: A partial frame times out when later read progress falls below the configured rate.
///
/// HOW:
/// - Echo a complete frame, then provide three bytes in one read period and only one in the next.
/// - Check the connection stays active during sufficient progress, then closes with a read-timeout stop.
#[ntex::test]
async fn read_timeout() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);

    let data = Arc::new(Mutex::new(RefCell::new(Vec::new())));
    let data2 = data.clone();

    let io = Io::new(
        server,
        SharedCfg::new("TEST").add(
            IoConfig::new()
                .set_keepalive_timeout(Seconds::ZERO)
                .set_frame_read_rate(Seconds(1), Seconds(2), 2),
        ),
    );

    let (disp, state) = Dispatcher::debug(
        io,
        BCodec(8),
        ntex_service::fn_service(move |msg: DispatchItem<BCodec>| {
            let data = data2.clone();
            async move {
                match msg {
                    DispatchItem::Item(bytes) => {
                        data.lock().unwrap().borrow_mut().push(0);
                        return Ok::<_, ()>(Some(bytes));
                    }
                    DispatchItem::Stop(Reason::ReadTimeout) => {
                        data.lock().unwrap().borrow_mut().push(1);
                    }
                    _ => (),
                }
                Ok(None)
            }
        }),
    );
    spawn(async move {
        let _ = disp.await;
    });

    client.write("12345678");
    let buf = client.read().await.unwrap();
    assert_eq!(buf, Bytes::from_static(b"12345678"));

    // the first period receives 3 bytes and is extended, the second one
    // receives 1 byte and fails
    client.write("1");
    sleep(Millis(500)).await;
    assert!(state.0.is_active());
    client.write("23");
    sleep(Millis(1000)).await;
    assert!(state.0.is_active());
    client.write("4");
    wait_until(Millis(3000), || {
        !state.0.is_active() && client.is_closed() && data.lock().unwrap().borrow().len() == 2
    })
    .await;

    // write side must be closed, dispatcher should fail with keep-alive
    assert!(!state.0.is_active());
    assert!(client.is_closed());
    assert_eq!(&data.lock().unwrap().borrow()[..], &[0, 1]);
}

/// STATEMENT: An external timeout closes an idle connection even when keep-alive is disabled.
///
/// HOW:
/// - Echo a one-byte frame and schedule `IoRef::notify_timeout` from the service.
/// - Verify the echoed byte, then wait for the transport to become inactive and the peer to close.
#[ntex::test]
async fn idle_timeout() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);

    let data = Arc::new(Mutex::new(RefCell::new(Vec::new())));
    let data2 = data.clone();

    let io = Io::new(
        server,
        SharedCfg::new("DBG").add(IoConfig::new().set_keepalive_timeout(Seconds::ZERO)),
    );
    let ioref = io.get_ref();

    let (disp, state) = Dispatcher::debug(
        io,
        BCodec(1),
        ntex_service::fn_service(move |msg: DispatchItem<BCodec>| {
            let ioref = ioref.clone();
            ntex::rt::spawn(async move {
                sleep(Millis(500)).await;
                ioref.notify_timeout();
            });
            let data = data2.clone();
            async move {
                match msg {
                    DispatchItem::Item(bytes) => {
                        data.lock().unwrap().borrow_mut().push(0);
                        return Ok::<_, ()>(Some(bytes));
                    }
                    DispatchItem::Stop(Reason::ReadTimeout) => {
                        data.lock().unwrap().borrow_mut().push(1);
                    }
                    _ => (),
                }
                Ok(None)
            }
        }),
    );
    spawn(async move {
        let _ = disp.await;
    });

    client.write("1");
    let buf = client.read().await.unwrap();
    assert_eq!(buf, Bytes::from_static(b"1"));

    wait_until(Millis(2000), || !state.0.is_active() && client.is_closed()).await;
    assert!(!state.0.is_active());
    assert!(client.is_closed());
}

/// STATEMENT: The dispatcher still invokes its service when the peer closes with input already buffered.
///
/// HOW:
/// - Write a request and close the peer before spawning the dispatcher.
/// - Let dispatch run and verify the flag set on service invocation becomes true.
#[ntex::test]
async fn unhandled_data() {
    let handled = Arc::new(AtomicBool::new(false));
    let handled2 = handled.clone();

    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);
    client.write("GET /test HTTP/1\r\n\r\n");

    let (disp, _) = Dispatcher::debug(
        Io::from(server),
        BytesCodec,
        ntex_service::fn_service(move |msg: DispatchItem<BytesCodec>| {
            handled2.store(true, Relaxed);
            async move {
                sleep(Millis(50)).await;
                if let DispatchItem::Item(msg) = msg {
                    Ok::<_, ()>(Some(msg))
                } else if let DispatchItem::Stop(_) = msg {
                    Ok::<_, ()>(None)
                } else {
                    panic!()
                }
            }
        }),
    );
    client.close().await;
    spawn(async move {
        let _ = disp.await;
    });
    sleep(Millis(50)).await;

    assert!(handled.load(Relaxed));
}

async fn stop_reason_at_eof(input: &'static str) -> Option<io::ErrorKind> {
    let reason = Rc::new(RefCell::new(None));
    let reason2 = reason.clone();

    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);
    client.write(input);

    let (disp, state) = Dispatcher::debug(
        Io::from(server),
        BCodec(8),
        ntex_service::fn_service(move |msg: DispatchItem<BCodec>| {
            if let DispatchItem::Stop(Reason::Io(err)) = msg {
                *reason2.borrow_mut() = Some(err.map(|e| e.kind()));
            }
            async move { Ok::<_, ()>(None) }
        }),
    );
    spawn(async move {
        let _ = disp.await;
    });
    sleep(Millis(25)).await;
    client.close().await;
    sleep(Millis(50)).await;

    let reason = reason.borrow_mut().take().expect("dispatcher did not stop");
    let close = if reason.is_some() {
        CloseReason::Io
    } else {
        CloseReason::PeerGone
    };
    assert_eq!(state.0.close_reason(), Some(close));
    reason
}

/// STATEMENT: Buffered frames are delivered before a transport failure's stop notification.
///
/// HOW:
/// - Send two eight-byte frames to a readiness-gated echo service and make writes fail with `ConnectionReset`.
/// - Release the gate and verify both frame payloads precede the stop event carrying the reset error.
#[ntex::test]
async fn peer_gone_during_pause_dispatches_buffered_frames() {
    struct Srv(Condition, Rc<RefCell<Vec<String>>>);

    impl Service<(), DispatchItem<BCodec>> for Srv {
        type Res = Option<Bytes>;
        type Error = ();

        async fn ready(&self, _: Ctx<'_, Self>) -> Result<(), Self::Error> {
            if self.1.borrow().len() == 1 {
                self.0.wait().await;
            }
            Ok(())
        }

        async fn call(
            &self,
            msg: DispatchItem<BCodec>,
            _: Ctx<'_, Self>,
        ) -> Result<Option<Bytes>, Self::Error> {
            match msg {
                DispatchItem::Item(msg) => {
                    self.1
                        .borrow_mut()
                        .push(String::from_utf8_lossy(&msg).into_owned());
                    return Ok(Some(msg));
                }
                DispatchItem::Stop(Reason::Io(err)) => self
                    .1
                    .borrow_mut()
                    .push(format!("stop: {:?}", err.map(|e| e.kind()))),
                _ => (),
            }
            Ok(None)
        }
    }

    let gate = Condition::new();
    let items = Rc::new(RefCell::new(Vec::new()));
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);
    // the response to the first frame fails while the service is paused
    client.write_error(io::Error::new(io::ErrorKind::ConnectionReset, "reset"));
    client.write("1234567887654321");

    let (disp, _) = Dispatcher::debug(
        Io::from(server),
        BCodec(8),
        Srv(gate.clone(), items.clone()),
    );
    spawn(async move {
        let _ = disp.await;
    });
    sleep(Millis(25)).await;

    gate.notify(());
    sleep(Millis(50)).await;

    assert_eq!(
        *items.borrow(),
        ["12345678", "87654321", "stop: Some(ConnectionReset)"]
    );
}

/// STATEMENT: A service error while handling the stop event becomes the dispatcher result.
///
/// HOW:
/// - Close the peer of a service that returns an error only for `DispatchItem::Stop`.
/// - Await the dispatcher and verify it returns that stop-handler error.
#[ntex::test]
async fn stop_call_error_is_returned() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);

    let (disp, _) = Dispatcher::debug(
        Io::from(server),
        BCodec(8),
        ntex_service::fn_service(async move |msg: DispatchItem<BCodec>| match msg {
            DispatchItem::Stop(_) => Err("stop"),
            _ => Ok(None),
        }),
    );
    client.close().await;
    assert_eq!(disp.await, Err("stop"));
}

/// STATEMENT: An earlier item-handler error takes precedence over a later stop-handler error.
///
/// HOW:
/// - Send a complete frame to a service with distinct errors for item and stop handling.
/// - Await the dispatcher and verify its result retains the item error.
#[ntex::test]
async fn earlier_service_error_is_kept() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);
    client.write("12345678");

    let (disp, _) = Dispatcher::debug(
        Io::from(server),
        BCodec(8),
        ntex_service::fn_service(async move |msg: DispatchItem<BCodec>| match msg {
            DispatchItem::Item(_) => Err("item"),
            DispatchItem::Stop(_) => Err("stop"),
            DispatchItem::Control(_) => Ok(None),
        }),
    );
    assert_eq!(disp.await, Err("item"));
}

/// STATEMENT: Peer EOF with a partial frame reports truncation rather than a clean disconnect.
///
/// HOW:
/// - Send three bytes through an eight-byte codec and close the peer using `stop_reason_at_eof`.
/// - Check the helper observes `CloseReason::Io` and returns `UnexpectedEof`.
#[ntex::test]
async fn peer_eof_reports_truncated_frame() {
    assert_eq!(
        stop_reason_at_eof("123").await,
        Some(io::ErrorKind::UnexpectedEof)
    );
}

/// STATEMENT: Peer EOF after a complete frame is a clean disconnect.
///
/// HOW:
/// - Send one complete eight-byte frame and close the peer using `stop_reason_at_eof`.
/// - Verify no I/O error is returned and the helper observes `CloseReason::PeerGone`.
#[ntex::test]
async fn peer_eof_after_whole_frame_is_clean() {
    assert_eq!(stop_reason_at_eof("12345678").await, None);
}

/// STATEMENT: Responses and write-backpressure notifications survive a service-readiness gate.
///
/// HOW:
/// - Use a one-shot readiness gate and a two-byte write watermark while sending nine-byte requests.
/// - Release the gate and output capacity, then check the echoed bytes and two backpressure-enabled events.
#[ntex::test]
async fn service_is_not_ready_and_backpressure() {
    struct Srv(
        Cell<Option<oneshot::Receiver<()>>>,
        Rc<Cell<usize>>,
        Cell<bool>,
    );

    impl Service<(), DispatchItem<BytesCodec>> for Srv {
        type Res = Option<Bytes>;
        type Error = ();

        async fn ready(&self, _: Ctx<'_, Self>) -> Result<(), Self::Error> {
            if self.2.get()
                && let Some(rx) = self.0.take()
            {
                let _ = rx.await;
            }
            Ok(())
        }

        async fn call(
            &self,
            msg: DispatchItem<BytesCodec>,
            _: Ctx<'_, Self>,
        ) -> Result<Option<Bytes>, Self::Error> {
            if let DispatchItem::Item(msg) = msg {
                self.2.set(true);
                return Ok::<_, ()>(Some(msg));
            } else if let DispatchItem::Control(Control::WBackPressureEnabled) = msg {
                self.1.set(self.1.get() + 1);
            }
            Ok::<_, ()>(None)
        }
    }

    let (ctx, rx) = oneshot::channel();
    let cnt = Rc::new(Cell::new(0));
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(0);

    let (disp, _) = Dispatcher::debug(
        Io::new(
            server,
            SharedCfg::new("DBG").add(IoConfig::new().set_write_backpressure(2)),
        ),
        BytesCodec,
        Srv(Cell::new(Some(rx)), cnt.clone(), Cell::new(false)),
    );
    let (tx, rx) = ntex::channel::oneshot::channel();
    ntex_util::spawn(async move {
        let _ = disp.await;
        let _ = tx.send(());
    });

    client.write("123456789");
    client.remote_buffer_cap(9);
    sleep(Millis(125)).await;
    let _ = ctx.send(());
    client.write("123456789");
    sleep(Millis(125)).await;
    client.remote_buffer_cap(16);
    let res = client.read().await;
    assert_eq!(res.unwrap(), Bytes::from_static(b"123456789"));
    client.close().await;
    let _ = rx.await;
    assert_eq!(cnt.get(), 2);
}

/// STATEMENT: Completing a frame stops its read timer rather than timing out the subsequent idle connection.
///
/// HOW:
/// - Disable keep-alive and send an eight-byte frame in two halves with frame-read tracking enabled.
/// - Verify the echo, wait beyond the initial read period, and check the peer remains open without a stop event.
#[ntex::test]
async fn read_timer_stopped_after_frame() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);

    let data = Arc::new(Mutex::new(RefCell::new(Vec::new())));
    let data2 = data.clone();

    let io = Io::new(
        server,
        SharedCfg::new("TEST").add(
            IoConfig::new()
                .set_keepalive_timeout(Seconds::ZERO)
                .set_frame_read_rate(Seconds(1), Seconds(5), 2),
        ),
    );
    let disp = Dispatcher::new(
        io,
        BCodec(8),
        Pipeline::new(
            (),
            ntex_service::fn_service(move |msg: DispatchItem<BCodec>| {
                let data = data2.clone();
                async move {
                    match msg {
                        DispatchItem::Item(bytes) => {
                            data.lock().unwrap().borrow_mut().push(0);
                            return Ok::<_, ()>(Some(bytes));
                        }
                        DispatchItem::Stop(_) => data.lock().unwrap().borrow_mut().push(1),
                        DispatchItem::Control(_) => (),
                    }
                    Ok(None)
                }
            }),
        ),
    );
    spawn(async move {
        let _ = disp.await;
    });

    client.write("1234");
    sleep(Millis(200)).await;
    client.write("5678");
    let buf = client.read().await.unwrap();
    assert_eq!(buf, Bytes::from_static(b"12345678"));

    sleep(Millis(2500)).await;
    assert!(!client.is_closed());
    assert_eq!(&data.lock().unwrap().borrow()[..], &[0]);
}

/// Drops buffered bytes up to and including `#`; frames are 8 bytes.
struct DropCodec;

impl Encoder for DropCodec {
    type Item = Bytes;
    type Error = io::Error;

    fn encode(&self, item: Bytes, dst: &mut BytePages) -> Result<(), Self::Error> {
        dst.append(item);
        Ok(())
    }
}

impl Decoder for DropCodec {
    type Item = Bytes;
    type Error = io::Error;

    fn decode(&self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if let Some(n) = src.iter().position(|b| *b == b'#') {
            let _ = src.split_to(n + 1);
        }
        if src.len() < 8 {
            Ok(None)
        } else {
            Ok(Some(src.split_to(8)))
        }
    }
}

/// STATEMENT: Codec-consumed bytes count as read progress even when no frame is produced.
///
/// HOW:
/// - Repeatedly send delimiter-terminated chunks that `DropCodec` consumes without yielding a frame.
/// - Verify the connection remains open past the initial read period with no timeout event.
/// - Stop sending and check that the connection eventually closes with one read-timeout notification.
#[ntex::test]
async fn read_rate_counts_consumed_bytes() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);

    let data = Arc::new(Mutex::new(RefCell::new(Vec::new())));
    let data2 = data.clone();

    let io = Io::new(
        server,
        SharedCfg::new("TEST").add(
            IoConfig::new()
                .set_keepalive_timeout(Seconds::ZERO)
                .set_frame_read_rate(Seconds(1), Seconds(10), 2),
        ),
    );
    let disp = Dispatcher::new(
        io,
        DropCodec,
        Pipeline::new(
            (),
            ntex_service::fn_service(move |msg: DispatchItem<DropCodec>| {
                let data = data2.clone();
                async move {
                    if let DispatchItem::Stop(Reason::ReadTimeout) = msg {
                        data.lock().unwrap().borrow_mut().push(1);
                    }
                    Ok::<_, ()>(None)
                }
            }),
        ),
    );
    spawn(async move {
        let _ = disp.await;
    });

    client.write("123");
    // several periods are extended by consumed bytes only
    for _ in 0..4 {
        sleep(Millis(350)).await;
        client.write("abc#");
    }
    // a period without consumed bytes must still be running, without the
    // consumed bytes the timer would have expired at 2s
    sleep(Millis(1000)).await;
    assert!(!client.is_closed());
    assert!(data.lock().unwrap().borrow().is_empty());

    // no progress, the frame read timer expires
    wait_closed(&client, Millis(4500)).await;
    assert!(client.is_closed());
    assert_eq!(&data.lock().unwrap().borrow()[..], &[1]);
}

fn first_frame_dispatcher(
    server: IoTest,
    data: Arc<Mutex<RefCell<Vec<usize>>>>,
) -> Dispatcher<BCodec, ()> {
    let io = Io::new(
        server,
        SharedCfg::new("TEST").add(
            IoConfig::new()
                .set_keepalive_timeout(Seconds(30))
                .set_frame_read_rate(Seconds(1), Seconds(2), 2),
        ),
    );
    Dispatcher::new(
        io,
        BCodec(8),
        Pipeline::new(
            (),
            ntex_service::fn_service(move |msg: DispatchItem<BCodec>| {
                let data = data.clone();
                async move {
                    match msg {
                        DispatchItem::Item(bytes) => {
                            data.lock().unwrap().borrow_mut().push(0);
                            return Ok::<_, ()>(Some(bytes));
                        }
                        DispatchItem::Stop(Reason::ReadTimeout) => {
                            data.lock().unwrap().borrow_mut().push(1);
                        }
                        DispatchItem::Stop(Reason::KeepAlive) => {
                            data.lock().unwrap().borrow_mut().push(2);
                        }
                        _ => (),
                    }
                    Ok(None)
                }
            }),
        ),
    )
}

/// STATEMENT: Initial frame-read tracking closes a silent peer before its longer keep-alive timeout.
///
/// HOW:
/// - Start `first_frame_dispatcher` with a short read deadline and a thirty-second keep-alive timeout.
/// - Send nothing and verify peer closure within five seconds with only a read-timeout stop recorded.
#[ntex::test]
async fn first_frame_read_rate_silent_peer() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);

    let data = Arc::new(Mutex::new(RefCell::new(Vec::new())));
    let disp = first_frame_dispatcher(server, data.clone());
    spawn(async move {
        let _ = disp.await;
    });

    wait_until(Millis(5000), || {
        client.is_closed() && data.lock().unwrap().borrow().len() == 1
    })
    .await;
    assert!(client.is_closed());
    assert_eq!(&data.lock().unwrap().borrow()[..], &[1]);
}

/// STATEMENT: A completed first frame is not closed by its former read-rate deadline.
///
/// HOW:
/// - Send a complete frame shortly after starting a dispatcher with short read and long keep-alive timeouts.
/// - Verify its echo, wait past the read deadline, and check the peer stays open with only an item event.
#[ntex::test]
async fn first_frame_read_rate_then_keepalive() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);

    let data = Arc::new(Mutex::new(RefCell::new(Vec::new())));
    let disp = first_frame_dispatcher(server, data.clone());
    spawn(async move {
        let _ = disp.await;
    });

    sleep(Millis(200)).await;
    client.write("12345678");
    let buf = client.read().await.unwrap();
    assert_eq!(buf, Bytes::from_static(b"12345678"));

    // without stopping the frame read timer the connection would be
    // closed after the 1s period and its 1s extension
    sleep(Millis(2500)).await;
    assert!(!client.is_closed());
    assert_eq!(&data.lock().unwrap().borrow()[..], &[0]);
}

/// Readiness gate installed by the test.
#[derive(Clone, Default)]
struct Gate(Rc<(RefCell<Option<oneshot::Receiver<()>>>, ReadyNotifier)>);

impl Gate {
    /// Pauses readiness until `rx` resolves, invalidating cached readiness.
    fn close(&self, rx: oneshot::Receiver<()>) {
        *self.0.0.borrow_mut() = Some(rx);
        self.0.1.notify();
    }

    fn changed(&self) -> &ReadyNotifier {
        &self.0.1
    }

    async fn ready(&self) {
        let rx = self.0.0.borrow_mut().take();
        if let Some(rx) = rx {
            let _ = rx.await;
        }
    }
}

/// Service whose readiness waits on a gate installed by the test.
struct GateSrv(Gate, Rc<Cell<bool>>);

impl Service<(), DispatchItem<BCodec>> for GateSrv {
    type Res = Option<Bytes>;
    type Error = ();

    fn bind(&self, ctx: BindCtx<'_>) {
        ctx.subscribe(self.0.changed());
    }

    async fn ready(&self, _: Ctx<'_, Self>) -> Result<(), Self::Error> {
        self.0.ready().await;
        Ok(())
    }

    async fn call(
        &self,
        msg: DispatchItem<BCodec>,
        _: Ctx<'_, Self>,
    ) -> Result<Option<Bytes>, Self::Error> {
        if let DispatchItem::Stop(Reason::ReadTimeout) = msg {
            self.1.set(true);
        }
        Ok(None)
    }
}

/// STATEMENT: Service pauses restore the partial frame's read-rate timeout budget.
///
/// HOW:
/// - Send partial-frame bytes across two timed cycles, briefly closing and releasing the readiness gate in each.
/// - Verify the connection remains open and no read-timeout stop is observed after both cycles.
#[ntex::test]
async fn read_rate_budget_reset_on_service_pause() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);

    let gate = Gate::default();
    let timed_out = Rc::new(Cell::new(false));
    let io = Io::new(
        server,
        SharedCfg::new("TEST").add(
            IoConfig::new()
                .set_keepalive_timeout(Seconds::ZERO)
                .set_frame_read_rate(Seconds(1), Seconds(2), 2),
        ),
    );
    let disp = Dispatcher::new(
        io,
        BCodec(1024),
        Pipeline::new((), GateSrv(gate.clone(), timed_out.clone())),
    );
    spawn(async move {
        let _ = disp.await;
    });

    // the budget allows one extension, at least two 1s periods, which
    // covers a 1.5s cycle. Without a restore by the pause it lasts less
    // than two 2s periods, two cycles take longer.
    client.write("abc");
    for _ in 0..2 {
        for _ in 0..2 {
            sleep(Millis(700)).await;
            client.write("abc");
        }
        let (tx, rx) = oneshot::channel();
        gate.close(rx);
        client.write("abc");
        sleep(Millis(100)).await;
        let _ = tx.send(());
    }
    assert!(!client.is_closed());
    assert!(!timed_out.get());
}

/// STATEMENT: Frame-read timeout monitoring resumes after a service pause.
///
/// HOW:
/// - Start a partial frame, pause service readiness with additional bytes buffered, and then release the gate.
/// - Send no more data and verify the peer initially stays open, then closes with a read-timeout notification.
#[ntex::test]
async fn read_rate_restarts_after_service_pause() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);

    let gate = Gate::default();
    let timed_out = Rc::new(Cell::new(false));
    let io = Io::new(
        server,
        SharedCfg::new("TEST").add(
            IoConfig::new()
                .set_keepalive_timeout(Seconds::ZERO)
                .set_frame_read_rate(Seconds(1), Seconds::ZERO, 2),
        ),
    );
    let disp = Dispatcher::new(
        io,
        BCodec(1024),
        Pipeline::new((), GateSrv(gate.clone(), timed_out.clone())),
    );
    spawn(async move {
        let _ = disp.await;
    });

    client.write("abc");
    sleep(Millis(100)).await;
    let (tx, rx) = oneshot::channel();
    gate.close(rx);
    client.write("abc");
    sleep(Millis(1500)).await;
    let _ = tx.send(());

    // no more data after the pause, the data sent during the pause
    // extends the first period
    sleep(Millis(1500)).await;
    assert!(!client.is_closed());
    wait_closed(&client, Millis(3500)).await;
    assert!(client.is_closed());
    assert!(timed_out.get());
}

/// STATEMENT: Consuming input without producing a frame still starts frame-read timeout tracking.
///
/// HOW:
/// - Deliver a complete frame, then send bytes that `DropCodec` discards entirely without yielding another.
/// - Verify the peer closes before the long keep-alive deadline and records an item followed by a read timeout.
#[ntex::test]
async fn read_rate_starts_for_consumed_partial_frame() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);

    let data = Arc::new(Mutex::new(RefCell::new(Vec::new())));
    let data2 = data.clone();

    let io = Io::new(
        server,
        SharedCfg::new("TEST").add(
            IoConfig::new()
                .set_keepalive_timeout(Seconds(30))
                .set_frame_read_rate(Seconds(1), Seconds(2), 2),
        ),
    );
    let disp = Dispatcher::new(
        io,
        DropCodec,
        Pipeline::new(
            (),
            ntex_service::fn_service(move |msg: DispatchItem<DropCodec>| {
                let data = data2.clone();
                async move {
                    match msg {
                        DispatchItem::Item(_) => data.lock().unwrap().borrow_mut().push(0),
                        DispatchItem::Stop(Reason::ReadTimeout) => {
                            data.lock().unwrap().borrow_mut().push(1);
                        }
                        _ => (),
                    }
                    Ok::<_, ()>(None)
                }
            }),
        ),
    );
    spawn(async move {
        let _ = disp.await;
    });

    client.write("12345678");
    sleep(Millis(100)).await;
    // the codec consumes the whole input without producing a frame
    client.write("1#");
    wait_until(Millis(5000), || {
        client.is_closed() && data.lock().unwrap().borrow().len() == 2
    })
    .await;
    assert!(client.is_closed());
    assert_eq!(&data.lock().unwrap().borrow()[..], &[0, 1]);
}

/// Service that is not ready while a frame is handled.
struct BusySrv(Rc<Cell<bool>>, IoRef);

impl Service<(), DispatchItem<BCodec>> for BusySrv {
    type Res = Option<Bytes>;
    type Error = ();

    async fn ready(&self, _: Ctx<'_, Self>) -> Result<(), Self::Error> {
        while self.0.get() {
            sleep(Millis(50)).await;
        }
        Ok(())
    }

    async fn call(
        &self,
        msg: DispatchItem<BCodec>,
        _: Ctx<'_, Self>,
    ) -> Result<Option<Bytes>, Self::Error> {
        if let DispatchItem::Item(bytes) = msg {
            self.0.set(true);
            let ioref = self.1.clone();
            spawn(async move {
                sleep(Millis(300)).await;
                ioref.notify_timeout();
            });
            sleep(Millis(800)).await;
            self.0.set(false);
            return Ok(Some(bytes));
        }
        Ok(None)
    }
}

/// STATEMENT: An external timeout during active frame handling is ignored rather than deferred.
///
/// HOW:
/// - Handle a frame for 800 milliseconds and issue `IoRef::notify_timeout` partway through the call.
/// - Verify the peer stays open during handling, receives the echo, and remains open after completion.
#[ntex::test]
async fn notify_timeout_ignored_during_pause_while_handling() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);

    let io = Io::new(
        server,
        SharedCfg::new("TEST").add(IoConfig::new().set_keepalive_timeout(Seconds(5))),
    );
    let srv = BusySrv(Rc::new(Cell::new(false)), io.get_ref());
    let disp = Dispatcher::new(io, BCodec(1), Pipeline::new((), srv));
    spawn(async move {
        let _ = disp.await;
    });

    client.write("1");
    // the timeout is delivered at 300ms, the frame is handled for 800ms
    sleep(Millis(600)).await;
    assert!(!client.is_closed());
    let buf = client.read().await.unwrap();
    assert_eq!(buf, Bytes::from_static(b"1"));
    // the timeout is not delivered after the service is ready again
    sleep(Millis(300)).await;
    assert!(!client.is_closed());
}

/// STATEMENT: An external timeout during an idle service pause still closes the connection.
///
/// HOW:
/// - Block initial service readiness with keep-alive disabled and issue `IoRef::notify_timeout`.
/// - Release the readiness gate and verify the peer closes.
#[ntex::test]
async fn notify_timeout_during_idle_pause() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);

    let gate = Gate::default();
    let io = Io::new(
        server,
        SharedCfg::new("TEST").add(IoConfig::new().set_keepalive_timeout(Seconds::ZERO)),
    );
    let ioref = io.get_ref();
    let (tx, rx) = oneshot::channel();
    gate.close(rx);
    let disp = Dispatcher::new(
        io,
        BCodec(1),
        Pipeline::new((), GateSrv(gate, Rc::new(Cell::new(false)))),
    );
    spawn(async move {
        let _ = disp.await;
    });

    sleep(Millis(300)).await;
    ioref.notify_timeout();
    sleep(Millis(300)).await;
    // the stop item is delivered once the service is ready
    let _ = tx.send(());
    wait_closed(&client, Millis(1000)).await;
    assert!(client.is_closed());
}

fn keepalive_dispatcher(
    server: IoTest,
    delay: Millis,
    data: Arc<Mutex<RefCell<Vec<usize>>>>,
) -> Dispatcher<BCodec, ()> {
    keepalive_io_dispatcher(keepalive_io(server), delay, data)
}

fn keepalive_io(server: IoTest) -> Io {
    Io::new(
        server,
        SharedCfg::new("TEST").add(IoConfig::new().set_keepalive_timeout(Seconds(1))),
    )
}

fn keepalive_io_dispatcher(
    io: Io,
    delay: Millis,
    data: Arc<Mutex<RefCell<Vec<usize>>>>,
) -> Dispatcher<BCodec, ()> {
    Dispatcher::new(
        io,
        BCodec(8),
        Pipeline::new(
            (),
            ntex_service::fn_service(move |msg: DispatchItem<BCodec>| {
                let data = data.clone();
                async move {
                    match msg {
                        DispatchItem::Item(bytes) => {
                            sleep(delay).await;
                            data.lock().unwrap().borrow_mut().push(0);
                            return Ok::<_, ()>(Some(bytes));
                        }
                        DispatchItem::Stop(Reason::KeepAlive) => {
                            data.lock().unwrap().borrow_mut().push(1);
                        }
                        _ => (),
                    }
                    Ok(None)
                }
            }),
        ),
    )
}

/// STATEMENT: Keep-alive does not expire an active service call and resumes after its response.
///
/// HOW:
/// - Delay a response beyond the one-second keep-alive timeout.
/// - Verify the peer remains open after handling, then closes with an item followed by a keep-alive stop.
#[ntex::test]
async fn keepalive_inactive_during_frame_handling() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);

    let data = Arc::new(Mutex::new(RefCell::new(Vec::new())));
    // an active 1s keep-alive would expire in less than 2s
    let disp = keepalive_dispatcher(server, Millis(2200), data.clone());
    spawn(async move {
        let _ = disp.await;
    });

    client.write("12345678");
    sleep(Millis(2400)).await;
    assert!(!client.is_closed());
    assert_eq!(&data.lock().unwrap().borrow()[..], &[0]);

    wait_closed(&client, Millis(3000)).await;
    assert!(client.is_closed());
    assert_eq!(&data.lock().unwrap().borrow()[..], &[0, 1]);
}

/// STATEMENT: Partial-frame reading suspends keep-alive expiry until the frame completes.
///
/// HOW:
/// - Send half a frame and wait longer than the configured keep-alive interval, checking the peer stays open.
/// - Complete the frame, verify its echo, and observe a subsequent idle close with a keep-alive stop.
#[ntex::test]
async fn keepalive_inactive_during_frame_read() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);

    let data = Arc::new(Mutex::new(RefCell::new(Vec::new())));
    let disp = keepalive_dispatcher(server, Millis(0), data.clone());
    spawn(async move {
        let _ = disp.await;
    });

    // keep-alive is armed for the idle connection, then a frame starts,
    // the 1s keep-alive would expire in less than 2s
    sleep(Millis(200)).await;
    client.write("1234");
    sleep(Millis(2200)).await;
    assert!(!client.is_closed());

    client.write("5678");
    let buf = client.read().await.unwrap();
    assert_eq!(buf, Bytes::from_static(b"12345678"));

    wait_closed(&client, Millis(3000)).await;
    assert!(client.is_closed());
    assert_eq!(&data.lock().unwrap().borrow()[..], &[0, 1]);
}

/// STATEMENT: Frame handlers can observe an armed keep-alive timer across successive requests.
///
/// HOW:
/// - Record `IoRef::timer_handle` while echoing two separate one-byte frames.
/// - Verify both responses, two set timer handles, and an open connection.
#[ntex::test]
async fn keepalive_timer_kept_during_frame_handling() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);

    let io = Io::new(
        server,
        SharedCfg::new("TEST").add(IoConfig::new().set_keepalive_timeout(Seconds(5))),
    );
    let ioref = io.get_ref();
    let handles = Rc::new(RefCell::new(Vec::new()));
    let handles2 = handles.clone();
    let disp = Dispatcher::new(
        io,
        BCodec(1),
        Pipeline::new(
            (),
            ntex_service::fn_service(move |msg: DispatchItem<BCodec>| {
                let handles = handles2.clone();
                let ioref = ioref.clone();
                async move {
                    if let DispatchItem::Item(bytes) = msg {
                        handles.borrow_mut().push(ioref.timer_handle());
                        return Ok::<_, ()>(Some(bytes));
                    }
                    Ok(None)
                }
            }),
        ),
    );
    spawn(async move {
        let _ = disp.await;
    });

    for frame in ["1", "2"] {
        sleep(Millis(50)).await;
        client.write(frame);
        let buf = client.read().await.unwrap();
        assert_eq!(buf, Bytes::from(frame));
    }
    assert_eq!(handles.borrow().len(), 2);
    assert!(handles.borrow().iter().all(ntex_io::TimerHandle::is_set));
    assert!(!client.is_closed());
}

/// STATEMENT: An external timeout can stop partial-frame reading even while keep-alive expiry is suspended.
///
/// HOW:
/// - Start an incomplete frame and confirm the peer remains open.
/// - Issue `IoRef::notify_timeout`, then verify closure and one keep-alive stop notification.
#[ntex::test]
async fn notify_timeout_during_frame_read() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);

    let io = Io::new(
        server,
        SharedCfg::new("TEST").add(IoConfig::new().set_keepalive_timeout(Seconds(5))),
    );
    let ioref = io.get_ref();
    let data = Rc::new(RefCell::new(Vec::new()));
    let data2 = data.clone();
    let disp = Dispatcher::new(
        io,
        BCodec(8),
        Pipeline::new(
            (),
            ntex_service::fn_service(move |msg: DispatchItem<BCodec>| {
                if let DispatchItem::Stop(Reason::KeepAlive) = msg {
                    data2.borrow_mut().push(1);
                }
                async move { Ok::<_, ()>(None) }
            }),
        ),
    );
    spawn(async move {
        let _ = disp.await;
    });

    sleep(Millis(100)).await;
    client.write("1234");
    sleep(Millis(100)).await;
    assert!(!client.is_closed());

    ioref.notify_timeout();
    wait_closed(&client, Millis(1000)).await;
    assert!(client.is_closed());
    assert_eq!(&data.borrow()[..], &[1]);
}

/// Service that answers every frame with `size` bytes and records events.
struct WriteSrv {
    size: usize,
    gate: Gate,
    events: Events,
}

impl Service<(), DispatchItem<BCodec>> for WriteSrv {
    type Res = Option<Bytes>;
    type Error = ();

    fn bind(&self, ctx: BindCtx<'_>) {
        ctx.subscribe(self.gate.changed());
    }

    async fn ready(&self, _: Ctx<'_, Self>) -> Result<(), Self::Error> {
        self.gate.ready().await;
        Ok(())
    }

    async fn call(
        &self,
        msg: DispatchItem<BCodec>,
        _: Ctx<'_, Self>,
    ) -> Result<Option<Bytes>, Self::Error> {
        let ev = match msg {
            DispatchItem::Item(_) => {
                self.events.borrow_mut().push("item");
                return Ok(Some(Bytes::from(vec![b'x'; self.size])));
            }
            DispatchItem::Control(Control::WBackPressureEnabled) => "bp-on",
            DispatchItem::Control(Control::WBackPressureDisabled) => "bp-off",
            DispatchItem::Stop(Reason::WriteTimeout) => "write-timeout",
            DispatchItem::Stop(Reason::KeepAlive) => "keepalive",
            DispatchItem::Stop(Reason::ReadTimeout) => "read-timeout",
            DispatchItem::Stop(_) => "stop",
        };
        self.events.borrow_mut().push(ev);
        Ok(None)
    }
}

type Events = Rc<RefCell<Vec<&'static str>>>;

fn write_dispatcher(
    server: IoTest,
    cfg: IoConfig,
    size: usize,
) -> (Dispatcher<BCodec, ()>, Gate, Events) {
    let gate = Gate::default();
    let events = Rc::new(RefCell::new(Vec::new()));
    let io = Io::new(
        server,
        SharedCfg::new("TEST").add(cfg.set_write_backpressure(1024)),
    );
    let disp = Dispatcher::new(
        io,
        BCodec(8),
        Pipeline::new(
            (),
            WriteSrv {
                size,
                gate: gate.clone(),
                events: events.clone(),
            },
        ),
    );
    (disp, gate, events)
}

/// STATEMENT: A peer that never drains write backpressure is stopped with a write timeout.
///
/// HOW:
/// - Block an 8,192-byte response with both keep-alive and write timeouts configured.
/// - Check the connection is initially open, then closes with item, backpressure-on, and write-timeout events.
#[ntex::test]
async fn write_timeout_peer_not_reading() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(0);

    let (disp, _, events) = write_dispatcher(
        server,
        IoConfig::new()
            .set_keepalive_timeout(Seconds(1))
            .set_write_timeout(Seconds(2)),
        8192,
    );
    spawn(async move {
        let _ = disp.await;
    });

    client.write("12345678");
    sleep(Millis(1500)).await;
    assert!(!client.is_closed());
    wait_closed(&client, Millis(4000)).await;
    assert!(client.is_closed());
    assert_eq!(&events.borrow()[..], &["item", "bp-on", "write-timeout"]);
}

/// STATEMENT: Small reads do not extend a write timeout while backpressure remains enabled.
///
/// HOW:
/// - Queue a 65,536-byte response and let the peer drain only 64 bytes at a time.
/// - Verify the connection closes and records item, backpressure-on, and write-timeout events in order.
#[ntex::test]
async fn write_timeout_slow_reader() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(0);

    let (disp, _, events) =
        write_dispatcher(server, IoConfig::new().set_write_timeout(Seconds(2)), 65536);
    spawn(async move {
        let _ = disp.await;
    });

    client.write("12345678");
    for _ in 0..24 {
        sleep(Millis(250)).await;
        client.remote_buffer_cap(64);
        let _ = client.read_any();
        if client.is_closed() {
            break;
        }
    }
    assert!(client.is_closed());
    wait_until(Millis(1000), || events.borrow().len() == 3).await;
    assert_eq!(&events.borrow()[..], &["item", "bp-on", "write-timeout"]);
}

/// STATEMENT: A write timeout closes the transport even while service readiness delays its stop notification.
///
/// HOW:
/// - Enable write backpressure and block service readiness with a one-shot gate.
/// - Verify the peer closes while only item and backpressure-on events have been delivered.
/// - Release the gate and check the deferred write-timeout stop is then delivered.
#[ntex::test]
async fn write_timeout_during_service_pause() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(0);

    let (disp, gate, events) =
        write_dispatcher(server, IoConfig::new().set_write_timeout(Seconds(1)), 8192);
    spawn(async move {
        let _ = disp.await;
    });

    client.write("12345678");
    sleep(Millis(100)).await;
    assert_eq!(&events.borrow()[..], &["item", "bp-on"]);

    // the service is not ready for a while
    let (tx, rx) = oneshot::channel::<()>();
    gate.close(rx);

    // io is closed without waiting for service readiness
    wait_closed(&client, Millis(5000)).await;
    assert!(client.is_closed());
    assert_eq!(&events.borrow()[..], &["item", "bp-on"]);

    // the stop item is delivered once the service is ready
    drop(tx);
    wait_until(Millis(1000), || events.borrow().len() == 3).await;
    assert_eq!(&events.borrow()[..], &["item", "bp-on", "write-timeout"]);
}

/// STATEMENT: Draining backpressure during a service pause cancels the write timeout before readiness resumes.
///
/// HOW:
/// - Block readiness after backpressure starts, then drain the entire response.
/// - Wait past the write deadline, release the gate, and verify an open peer and a backpressure-off event.
#[ntex::test]
async fn write_timeout_released_during_service_pause() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(0);

    let (disp, gate, events) =
        write_dispatcher(server, IoConfig::new().set_write_timeout(Seconds(1)), 8192);
    spawn(async move {
        let _ = disp.await;
    });

    client.write("12345678");
    sleep(Millis(100)).await;
    assert_eq!(&events.borrow()[..], &["item", "bp-on"]);

    // the service is not ready while the peer drains the output
    let (tx, rx) = oneshot::channel::<()>();
    gate.close(rx);
    client.remote_buffer_cap(65536);
    sleep(Millis(100)).await;
    assert_eq!(client.read_any().len(), 8192);
    // a write timeout that was not stopped would fire within 2s
    sleep(Millis(2300)).await;

    drop(tx);
    sleep(Millis(100)).await;
    assert!(!client.is_closed());
    assert_eq!(&events.borrow()[..], &["item", "bp-on", "bp-off"]);
}

/// STATEMENT: Keep-alive monitoring resumes once write backpressure is released.
///
/// HOW:
/// - Queue a response against a blocked peer, then drain it before the write timeout.
/// - Verify backpressure turns off and subsequent inactivity closes the peer with a keep-alive stop.
#[ntex::test]
async fn keepalive_after_write_backpressure() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(0);

    let (disp, _, events) = write_dispatcher(
        server,
        IoConfig::new()
            .set_keepalive_timeout(Seconds(1))
            .set_write_timeout(Seconds(1)),
        8192,
    );
    spawn(async move {
        let _ = disp.await;
    });

    client.write("12345678");
    wait_until(Millis(2000), || events.borrow().len() == 2).await;
    client.remote_buffer_cap(65536);
    sleep(Millis(100)).await;
    assert_eq!(client.read_any().len(), 8192);
    assert_eq!(&events.borrow()[..], &["item", "bp-on", "bp-off"]);

    wait_closed(&client, Millis(4000)).await;
    assert!(client.is_closed());
    assert_eq!(
        &events.borrow()[..],
        &["item", "bp-on", "bp-off", "keepalive"]
    );
}

/// STATEMENT: Releasing write backpressure stops its timeout even when some output remains buffered.
///
/// HOW:
/// - Drain 7,900 bytes of an 8,192-byte response and stop reading after backpressure turns off.
/// - Wait past the write deadline and verify the peer stays open without any further service events.
#[ntex::test]
async fn write_timeout_ends_at_backpressure_release() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(0);

    let (disp, _, events) =
        write_dispatcher(server, IoConfig::new().set_write_timeout(Seconds(1)), 8192);
    spawn(async move {
        let _ = disp.await;
    });

    client.write("12345678");
    sleep(Millis(250)).await;
    // drain below the release threshold, then stop reading
    client.remote_buffer_cap(7900);
    sleep(Millis(100)).await;
    assert_eq!(client.read_any().len(), 7900);
    assert_eq!(&events.borrow()[..], &["item", "bp-on", "bp-off"]);

    // a write timeout that was not stopped would fire within 2s
    sleep(Millis(2500)).await;
    assert!(!client.is_closed());
    assert_eq!(&events.borrow()[..], &["item", "bp-on", "bp-off"]);
}

/// STATEMENT: Each new write-backpressure period receives a fresh timeout budget.
///
/// HOW:
/// - Release two blocked responses within their deadlines, starting a new request after each drain.
/// - Verify the third backpressure period leaves the connection open with the expected item and pressure events.
#[ntex::test]
async fn write_timeout_restarts_per_backpressure_period() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(0);

    let (disp, _, events) =
        write_dispatcher(server, IoConfig::new().set_write_timeout(Seconds(2)), 8192);
    spawn(async move {
        let _ = disp.await;
    });

    client.write("12345678");
    for _ in 0..2 {
        // release backpressure within the timeout
        sleep(Millis(1200)).await;
        client.remote_buffer_cap(65536);
        sleep(Millis(100)).await;
        assert_eq!(client.read_any().len(), 8192);
        client.remote_buffer_cap(0);
        client.write("12345678");
    }
    sleep(Millis(100)).await;
    assert!(!client.is_closed());
    assert_eq!(
        &events.borrow()[..],
        &[
            "item", "bp-on", "bp-off", "item", "bp-on", "bp-off", "item", "bp-on"
        ]
    );
}

/// Service with a slow item handler and a large response.
struct SlowWriteSrv(Events);

impl Service<(), DispatchItem<BCodec>> for SlowWriteSrv {
    type Res = Option<Bytes>;
    type Error = ();

    async fn call(
        &self,
        msg: DispatchItem<BCodec>,
        _: Ctx<'_, Self>,
    ) -> Result<Option<Bytes>, Self::Error> {
        let ev = match msg {
            DispatchItem::Item(_) => {
                sleep(Millis(300)).await;
                self.0.borrow_mut().push("item");
                return Ok(Some(Bytes::from(vec![b'x'; 8192])));
            }
            DispatchItem::Control(Control::WBackPressureEnabled) => "bp-on",
            DispatchItem::Control(Control::WBackPressureDisabled) => "bp-off",
            DispatchItem::Stop(Reason::ReadTimeout) => "read-timeout",
            DispatchItem::Stop(_) => "stop",
        };
        self.0.borrow_mut().push(ev);
        Ok(None)
    }
}

/// STATEMENT: Frame-read timeout notifications are suppressed while write backpressure prevents decoding.
///
/// HOW:
/// - Start a partial second frame during a slow first call whose large response enables backpressure.
/// - Keep sending partial-frame bytes while writes remain blocked and verify no read-timeout event appears.
#[ntex::test]
async fn read_rate_stopped_during_write_backpressure() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(0);

    let events: Events = Rc::new(RefCell::new(Vec::new()));
    let io = Io::new(
        server,
        SharedCfg::new("TEST").add(
            IoConfig::new()
                .set_write_backpressure(1024)
                .set_frame_read_rate(Seconds(1), Seconds::ZERO, 0),
        ),
    );
    let disp = Dispatcher::new(
        io,
        BCodec(8),
        Pipeline::new((), SlowWriteSrv(events.clone())),
    );
    spawn(async move {
        let _ = disp.await;
    });

    // a partial frame arrives while the first frame is handled, then
    // the response enables write backpressure
    client.write("12345678");
    sleep(Millis(100)).await;
    client.write("1");
    sleep(Millis(400)).await;
    assert_eq!(&events.borrow()[..], &["item", "bp-on"]);

    // the peer keeps sending partial frame bytes, a running 1s timer
    // would expire in less than 2s
    for _ in 0..5 {
        sleep(Millis(500)).await;
        client.write("1");
    }
    assert_eq!(&events.borrow()[..], &["item", "bp-on"]);
}

/// Codec that fails to encode, and to decode input containing `!`.
struct ErrCodec;

impl Encoder for ErrCodec {
    type Item = Bytes;
    type Error = io::Error;

    fn encode(&self, _: Bytes, _: &mut BytePages) -> Result<(), Self::Error> {
        Err(io::Error::other("encode"))
    }
}

impl Decoder for ErrCodec {
    type Item = Bytes;
    type Error = io::Error;

    fn decode(&self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if src.contains(&b'!') {
            Err(io::Error::other("decode"))
        } else if src.is_empty() {
            Ok(None)
        } else {
            Ok(Some(src.split_to(src.len())))
        }
    }
}

/// Runs an `ErrCodec` dispatcher that echoes items, returns the stop
/// reason delivered to the service.
async fn err_codec_stop(input: &'static str) -> String {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);
    client.write(input);

    let reason = Rc::new(RefCell::new(String::new()));
    let reason2 = reason.clone();
    let (disp, _) = Dispatcher::debug(
        Io::from(server),
        ErrCodec,
        ntex_service::fn_service(move |msg: DispatchItem<ErrCodec>| {
            let reason = reason2.clone();
            async move {
                match msg {
                    DispatchItem::Item(item) => return Ok::<_, ()>(Some(item)),
                    DispatchItem::Stop(r) => *reason.borrow_mut() = format!("{r:?}"),
                    DispatchItem::Control(_) => (),
                }
                Ok(None)
            }
        }),
    );
    assert!(format!("{disp:?}").contains("Dispatcher"));
    assert_eq!(timeout(Millis(1000), disp).await, Ok(Ok(())));
    reason.take()
}

/// STATEMENT: An encoding failure stops dispatch with an encoder-specific reason.
///
/// HOW:
/// - Echo input through `ErrCodec`, which fails when encoding the response.
/// - Verify the helper completes the dispatcher and the recorded reason names the encoder and its error.
#[ntex::test]
async fn encoder_error_stops_dispatcher() {
    let reason = err_codec_stop("data").await;
    assert!(reason.starts_with("Reason::Encoder("), "{reason}");
    assert!(reason.contains("encode"), "{reason}");
}

/// STATEMENT: A decoding failure stops dispatch with a decoder-specific reason.
///
/// HOW:
/// - Send the failure marker to `ErrCodec` through `err_codec_stop`.
/// - Verify the helper completes the dispatcher and the recorded reason names the decoder and its error.
#[ntex::test]
async fn decoder_error_stops_dispatcher() {
    let reason = err_codec_stop("!").await;
    assert!(reason.starts_with("Reason::Decoder("), "{reason}");
    assert!(reason.contains("decode"), "{reason}");
}

/// STATEMENT: Dispatch items and stop reasons retain descriptive `Debug` formatting.
///
/// HOW:
/// - Format item, control, and stop variants and compare their exact output strings.
/// - Check named stop reasons and the encoder/decoder error variant prefixes.
#[test]
fn debug_formats() {
    let item = DispatchItem::<BCodec>::Item(Bytes::from_static(b"x"));
    assert_eq!(format!("{item:?}"), "DispatchItem::Item(b\"x\")");
    let item = DispatchItem::<BCodec>::Control(Control::WBackPressureEnabled);
    assert_eq!(
        format!("{item:?}"),
        "DispatchItem::Control(WBackPressureEnabled)"
    );
    let item = DispatchItem::<BCodec>::Stop(Reason::KeepAlive);
    assert_eq!(format!("{item:?}"), "DispatchItem::Stop(Reason::KeepAlive)");

    for (reason, expected) in [
        (Reason::<BCodec>::Service, "Reason::Service"),
        (Reason::Io(None), "Reason::Io(None)"),
        (Reason::KeepAlive, "Reason::KeepAlive"),
        (Reason::ReadTimeout, "Reason::ReadTimeout"),
        (Reason::WriteTimeout, "Reason::WriteTimeout"),
    ] {
        assert_eq!(format!("{reason:?}"), expected);
    }
    let reason = Reason::<BCodec>::Encoder(io::Error::other("e"));
    assert!(format!("{reason:?}").starts_with("Reason::Encoder("));
    let reason = Reason::<BCodec>::Decoder(io::Error::other("d"));
    assert!(format!("{reason:?}").starts_with("Reason::Decoder("));
}

/// STATEMENT: A write failure under backpressure is delivered as an I/O stop event.
///
/// HOW:
/// - Queue a response until backpressure is enabled, then inject a write error and permit socket writes.
/// - Verify the dispatcher completes successfully and reports the error after the item and backpressure event.
#[ntex::test]
async fn write_error_during_backpressure() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(0);

    let events = Rc::new(RefCell::new(Vec::new()));
    let events2 = events.clone();
    let io = Io::new(
        server,
        SharedCfg::new("TEST").add(IoConfig::new().set_write_backpressure(1024)),
    );
    let (disp, _) = Dispatcher::debug(
        io,
        BCodec(1),
        ntex_service::fn_service(move |msg: DispatchItem<BCodec>| {
            let events = events2.clone();
            async move {
                let ev = match msg {
                    DispatchItem::Item(_) => {
                        events.borrow_mut().push("item".to_string());
                        return Ok::<_, ()>(Some(Bytes::from(vec![b'x'; 4096])));
                    }
                    DispatchItem::Control(c) => format!("{c:?}"),
                    DispatchItem::Stop(Reason::Io(Some(err))) => format!("io: {err}"),
                    DispatchItem::Stop(r) => format!("{r:?}"),
                };
                events.borrow_mut().push(ev);
                Ok(None)
            }
        }),
    );
    let hnd = spawn(disp);

    client.write("1");
    sleep(Millis(100)).await;
    assert_eq!(&events.borrow()[..], &["item", "WBackPressureEnabled"]);

    client.write_error(io::Error::other("write failed"));
    client.remote_buffer_cap(1024);
    assert!(matches!(timeout(Millis(1000), hnd).await, Ok(Ok(Ok(())))));
    assert_eq!(
        &events.borrow()[..],
        &["item", "WBackPressureEnabled", "io: write failed"]
    );
}

/// STATEMENT: Sufficient ongoing read progress cannot exceed the cumulative frame-read timeout budget.
///
/// HOW:
/// - Send three-byte chunks every 300 milliseconds without completing the large frame.
/// - Verify closure within 3.5 seconds and exactly one read-timeout stop notification.
#[ntex::test]
async fn read_rate_max_timeout_reached() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);

    let data = Arc::new(Mutex::new(RefCell::new(Vec::new())));
    let data2 = data.clone();
    let io = Io::new(
        server,
        SharedCfg::new("TEST").add(
            IoConfig::new()
                .set_keepalive_timeout(Seconds::ZERO)
                .set_frame_read_rate(Seconds(1), Seconds(2), 2),
        ),
    );
    let (disp, _) = Dispatcher::debug(
        io,
        BCodec(1024),
        ntex_service::fn_service(move |msg: DispatchItem<BCodec>| {
            let data = data2.clone();
            async move {
                if let DispatchItem::Stop(Reason::ReadTimeout) = msg {
                    data.lock().unwrap().borrow_mut().push(1);
                }
                Ok::<_, ()>(None)
            }
        }),
    );
    spawn(async move {
        let _ = disp.await;
    });

    let start = std::time::Instant::now();
    for _ in 0..12 {
        client.write("abc");
        sleep(Millis(300)).await;
        if client.is_closed() {
            break;
        }
    }
    wait_closed(&client, Millis(1000)).await;
    assert!(client.is_closed());
    assert!(start.elapsed() < std::time::Duration::from_millis(3500));
    assert_eq!(&data.lock().unwrap().borrow()[..], &[1]);
}

#[derive(Default)]
struct GateState {
    blocked: Cell<bool>,
    hold: Cell<bool>,
    waker: RefCell<Option<std::task::Waker>>,
}

/// Gate control, a blocked filter chain is not ready and holds output.
#[derive(Clone, Default)]
struct GateCtl(Rc<GateState>);

impl fmt::Debug for GateCtl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GateCtl").finish()
    }
}

impl GateCtl {
    /// Blocks the chain, it holds output and is not ready. The read task
    /// is woken to observe the change, as a filter would do.
    fn block(&self) {
        self.set(true);
    }

    fn unblock(&self) {
        self.set(false);
    }

    /// Holds output while the chain stays ready.
    fn hold(&self) {
        self.0.hold.set(true);
    }

    fn set(&self, blocked: bool) {
        self.0.blocked.set(blocked);
        self.0.hold.set(blocked);
        if let Some(waker) = self.0.waker.borrow_mut().take() {
            waker.wake();
        }
    }
}

/// Passes data through, holds output while the gate is blocked.
#[derive(Debug)]
struct GateLayer(GateCtl);

impl ntex_io::FilterLayer for GateLayer {
    fn process_read_buf(&self, buf: &ntex_io::FilterBuf<'_>) -> io::Result<()> {
        buf.with_read_buffers(|src, dst| {
            if let Some(src) = src {
                dst.extend_from_slice(src);
                src.clear();
            }
        });
        Ok(())
    }

    fn process_write_buf(&self, buf: &ntex_io::FilterBuf<'_>) -> io::Result<()> {
        if !self.0.0.hold.get() {
            buf.with_write_buffers(BytePages::move_to);
        }
        Ok(())
    }

    fn shutdown(&self, _: &ntex_io::FilterBuf<'_>) -> io::Result<Poll<()>> {
        Ok(Poll::Ready(()))
    }
}

struct GateFilter<F>(F, GateCtl);

impl<F: ntex_io::Filter> ntex_io::Filter for GateFilter<F> {
    fn query(&self, id: std::any::TypeId) -> Option<Box<dyn std::any::Any>> {
        self.0.query(id)
    }

    fn process_read_buf(&self, ctx: &mut ntex_io::FilterCtx<'_>) -> io::Result<()> {
        self.0.process_read_buf(ctx)
    }

    fn process_write_buf(&self, ctx: &mut ntex_io::FilterCtx<'_>) -> io::Result<()> {
        self.0.process_write_buf(ctx)
    }

    fn shutdown(&self, ctx: &mut ntex_io::FilterCtx<'_>) -> io::Result<Poll<()>> {
        self.0.shutdown(ctx)
    }

    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        *self.1.0.waker.borrow_mut() = Some(cx.waker().clone());
        if self.1.0.blocked.get() {
            Poll::Pending
        } else {
            self.0.poll_ready(cx)
        }
    }
}

fn gated_io(server: IoTest, cfg: impl Into<SharedCfg>, gate: &GateCtl) -> Io<impl ntex_io::Filter> {
    let g = gate.clone();
    Io::new(server, cfg)
        .add_filter(GateLayer(gate.clone()))
        .map_filter(move |f| GateFilter(f, g))
}

/// Echo dispatcher over a gated filter chain, records items as `0` and
/// stop reasons as `1`.
fn gated_dispatcher(server: IoTest, cfg: IoConfig, data: Rc<RefCell<Vec<u8>>>) -> (GateCtl, IoRef) {
    let gate = GateCtl::default();
    let io = gated_io(server, SharedCfg::new("TEST").add(cfg), &gate);
    let ioref = io.get_ref();

    let disp = Dispatcher::new(
        io,
        BCodec(8),
        Pipeline::new(
            (),
            ntex_service::fn_service(move |msg: DispatchItem<BCodec>| {
                let data = data.clone();
                async move {
                    match msg {
                        DispatchItem::Item(bytes) => {
                            data.borrow_mut().push(0);
                            return Ok::<_, ()>(Some(bytes));
                        }
                        DispatchItem::Stop(Reason::ReadTimeout | Reason::KeepAlive) => {
                            data.borrow_mut().push(1);
                        }
                        _ => (),
                    }
                    Ok(None)
                }
            }),
        ),
    );
    spawn(async move {
        let _ = disp.await;
    });
    (gate, ioref)
}

/// STATEMENT: A paused filter suspends frame-read expiry and preserves the frame for later dispatch.
///
/// HOW:
/// - Begin a frame, block the filter, and send the remaining bytes while it is paused.
/// - Wait beyond the read budget and verify the transport stays active with no service events.
/// - Unblock the filter and check the complete echo, cleared pause state, and one item event.
#[ntex::test]
async fn filter_pause_suspends_read_timeout() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);
    let data = Rc::new(RefCell::new(Vec::new()));
    let (gate, io) = gated_dispatcher(
        server,
        IoConfig::new()
            .set_keepalive_timeout(Seconds::ZERO)
            .set_frame_read_rate(Seconds(1), Seconds(2), 2),
        data.clone(),
    );

    // a partial frame arms the frame read timer
    client.write("1234");
    sleep(Millis(100)).await;

    // the peer sends the rest of the frame while the filter is not ready
    gate.block();
    client.write("5678");
    sleep(Millis(100)).await;
    assert!(io.is_filter_paused());

    // the frame read timer does not run during the pause
    sleep(Millis(3000)).await;
    assert!(io.is_active());
    assert!(data.borrow().is_empty());

    gate.unblock();
    let buf = client.read().await.unwrap();
    assert_eq!(buf, Bytes::from_static(b"12345678"));
    assert!(!io.is_filter_paused());
    assert_eq!(&data.borrow()[..], &[0]);
}

/// STATEMENT: A filter pause suspends keep-alive expiry, which resumes once reads continue.
///
/// HOW:
/// - Block the filter before sending a frame and wait beyond the keep-alive interval.
/// - Verify the transport stays active without dispatch, then unblock it and check the echo.
/// - Wait for resumed inactivity to close the connection with an item followed by a timeout stop.
#[ntex::test]
async fn filter_pause_suspends_keepalive() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);
    let data = Rc::new(RefCell::new(Vec::new()));
    let (gate, io) = gated_dispatcher(
        server,
        IoConfig::new()
            .set_shutdown_timeout(Seconds(1))
            .set_keepalive_timeout(Seconds(1)),
        data.clone(),
    );

    // the peer sends a frame while the filter is not ready
    sleep(Millis(100)).await;
    gate.block();
    client.write("12345678");
    sleep(Millis(100)).await;
    assert!(io.is_filter_paused());

    // keep-alive does not run during the pause
    sleep(Millis(2000)).await;
    assert!(io.is_active());
    assert!(data.borrow().is_empty());

    gate.unblock();
    let buf = client.read().await.unwrap();
    assert_eq!(buf, Bytes::from_static(b"12345678"));

    // keep-alive is armed again once reads resume
    wait_until(Millis(3000), || {
        !io.is_active() && client.is_closed() && data.borrow().len() == 2
    })
    .await;
    assert!(!io.is_active());
    assert!(client.is_closed());
    assert_eq!(&data.borrow()[..], &[0, 1]);
}

/// `write_dispatcher` over a gated filter chain.
fn gated_write_dispatcher(server: IoTest, cfg: IoConfig, size: usize) -> (GateCtl, IoRef, Events) {
    let gate = GateCtl::default();
    let events = Rc::new(RefCell::new(Vec::new()));
    let io = gated_io(
        server,
        SharedCfg::new("TEST").add(cfg.set_write_backpressure(1024)),
        &gate,
    );
    let ioref = io.get_ref();
    let disp = Dispatcher::new(
        io,
        BCodec(8),
        Pipeline::new(
            (),
            WriteSrv {
                size,
                gate: Gate::default(),
                events: events.clone(),
            },
        ),
    );
    spawn(async move {
        let _ = disp.await;
    });
    (gate, ioref, events)
}

/// STATEMENT: A filter pause suspends the write timeout until buffered output can drain.
///
/// HOW:
/// - Hold an 8,192-byte response in the filter to enable backpressure, then block filter readiness.
/// - Wait beyond the write deadline and verify active transport with no timeout event.
/// - Unblock the filter, read the full response, and verify the pause and backpressure both clear.
#[ntex::test]
async fn filter_pause_suspends_write_timeout() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(65536);
    let (gate, io, events) =
        gated_write_dispatcher(server, IoConfig::new().set_write_timeout(Seconds(1)), 8192);

    // the filter holds the response, write backpressure is enabled
    gate.hold();
    client.write("12345678");
    sleep(Millis(100)).await;
    assert_eq!(&events.borrow()[..], &["item", "bp-on"]);
    gate.block();
    sleep(Millis(100)).await;
    assert!(io.is_filter_paused());

    // the write timeout does not run during the pause
    sleep(Millis(2500)).await;
    assert!(io.is_active());
    assert_eq!(&events.borrow()[..], &["item", "bp-on"]);

    gate.unblock();
    let mut received = 0;
    while received < 8192 {
        received += client.read().await.unwrap().len();
    }
    assert!(!io.is_filter_paused());
    wait_until(Millis(1000), || events.borrow().len() == 3).await;
    assert_eq!(&events.borrow()[..], &["item", "bp-on", "bp-off"]);
}

/// STATEMENT: A write timeout restarts with a fresh budget after a filter pause.
///
/// HOW:
/// - Keep the peer from reading, hold a response, and pause the filter beyond the original write interval.
/// - Resume the filter and verify the connection initially stays active despite continuing backpressure.
/// - Wait for the renewed deadline and check peer closure with a write-timeout stop event.
#[ntex::test]
async fn write_timeout_restarts_after_filter_pause() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(0);
    let (gate, io, events) =
        gated_write_dispatcher(server, IoConfig::new().set_write_timeout(Seconds(1)), 8192);

    gate.hold();
    client.write("12345678");
    sleep(Millis(100)).await;
    gate.block();
    sleep(Millis(1500)).await;
    assert!(io.is_filter_paused());
    assert!(io.is_active());

    // the peer does not read, the timeout starts over once the filter resumes
    gate.unblock();
    sleep(Millis(500)).await;
    assert!(!io.is_filter_paused());
    assert!(io.is_active());
    wait_closed(&client, Millis(2000)).await;
    assert!(client.is_closed());
    assert_eq!(&events.borrow()[..], &["item", "bp-on", "write-timeout"]);
}
