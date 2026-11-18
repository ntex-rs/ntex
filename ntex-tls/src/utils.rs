//! Handshake driving, timeout mapping, and metadata normalization shared by TLS backends.
#![allow(dead_code)]
use std::{future::Future, io, pin::Pin, task::Context, task::Poll};

use ntex_error::Error;
use ntex_io::{Io, types::HttpProtocol};
use ntex_net::connect::ConnectError;
use ntex_service::cfg::Cfg;
use ntex_util::time::{Millis, Sleep};

use crate::TlsConfig;

/// Runs a handshake with a deadline, preserving its original result.
///
/// Zero disables the deadline. The deadline starts on the first poll.
/// Expiration returns `TimedOut`; dropping the returned future drops the
/// handshake, so no separate background handshake is left running.
pub(crate) fn with_timeout<F, R>(timeout: Millis, fut: F) -> WithTimeout<F>
where
    F: Future<Output = io::Result<R>>,
{
    WithTimeout {
        fut,
        timeout,
        delay: None,
    }
}

pin_project_lite::pin_project! {
    /// Future returned by [`with_timeout`].
    pub(crate) struct WithTimeout<F> {
        #[pin]
        fut: F,
        timeout: Millis,
        delay: Option<Sleep>,
    }
}

impl<F, R> Future for WithTimeout<F>
where
    F: Future<Output = io::Result<R>>,
{
    type Output = io::Result<R>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        if this.delay.is_none() && !this.timeout.is_zero() {
            *this.delay = Some(Sleep::new(*this.timeout));
        }

        if let Poll::Ready(res) = this.fut.poll(cx) {
            return Poll::Ready(res);
        }

        match this.delay {
            Some(delay) if delay.poll_elapsed(cx).is_ready() => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "TLS Handshake timeout",
            ))),
            _ => Poll::Pending,
        }
    }
}

/// Runs a client handshake with timeout and configuration-scoped diagnostics.
///
/// Maps TLS and timeout errors into `ConnectError` and attaches the TLS service
/// identity. Transport establishment must already have completed.
pub(crate) fn connect<'a, F, R>(cfg: &Cfg<TlsConfig>, host: &'a str, fut: F) -> TlsConnect<'a, F>
where
    F: Future<Output = io::Result<R>>,
{
    log::trace!("{}: TLS Handshake start for: {host:?}", cfg.tag());
    TlsConnect {
        fut: with_timeout(cfg.handshake_timeout(), fut),
        tag: cfg.tag(),
        service: cfg.service(),
        host,
    }
}

pin_project_lite::pin_project! {
    /// Future returned by [`connect`].
    pub(crate) struct TlsConnect<'a, F> {
        #[pin]
        fut: WithTimeout<F>,
        tag: &'static str,
        service: &'static str,
        host: &'a str,
    }
}

impl<F, R> Future for TlsConnect<'_, F>
where
    F: Future<Output = io::Result<R>>,
{
    type Output = Result<R, Error<ConnectError>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        match this.fut.poll(cx) {
            Poll::Ready(Ok(io)) => {
                log::trace!("{}: TLS Handshake success: {:?}", this.tag, this.host);
                Poll::Ready(Ok(io))
            }
            Poll::Ready(Err(e)) => {
                log::trace!("{}: TLS Handshake error: {e:?}", this.tag);
                Poll::Ready(Err(
                    Error::from(ConnectError::from(e)).with_service(this.service)
                ))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Drives a handshake performed by the filter.
///
/// The filter processes handshake records as they are read and written,
/// `state` returns `true` while negotiation is in progress and `false` on success.
/// The callback must release backend state borrows before returning.
///
/// Flushes pending output before checking state, and attempts a full alert flush
/// before returning a reported handshake error. A read carrying EOF gets one
/// final state check because it may also contain the last handshake flight.
/// This helper imposes neither a timeout nor a handshake-capacity limit.
pub(crate) async fn handshake<F>(
    io: &Io<F>,
    state: impl Fn() -> io::Result<bool>,
) -> io::Result<()> {
    let mut eof = false;
    loop {
        io.flush(false).await?;
        match state() {
            Ok(true) => {}
            Ok(false) => return Ok(()),
            Err(err) => {
                // make sure the alert reaches the peer before the io is dropped
                let _ = io.flush(true).await;
                return Err(err);
            }
        }
        if eof {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "disconnected"));
        }
        // The read that reports eof may also carry the peer's last handshake
        // flight, so the handshake state is checked once more before the eof
        // is treated as a failure.
        eof = io.read_notify().await?.is_none();
    }
}

/// Maps exactly `h2` to HTTP/2; absent or other ALPN values fall back to HTTP/1.
pub(crate) fn http_protocol(alpn: Option<&[u8]>) -> HttpProtocol {
    if alpn == Some(b"h2") {
        HttpProtocol::Http2
    } else {
        HttpProtocol::Http1
    }
}

/// Strips the port and IPv6 brackets from a connect host.
///
/// Accepts `host`, `host:port`, `[v6]`, `[v6]:port` and a bare `v6` address.
/// This is syntactic normalization, not hostname, port, or certificate validation.
pub(crate) fn server_name(host: &str) -> &str {
    if let Some(rest) = host.strip_prefix('[') {
        rest.split_once(']').map_or(host, |(ip, _)| ip)
    } else {
        match host.split_once(':') {
            Some((name, port)) if !port.contains(':') => name,
            _ => host,
        }
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use std::{
        cell::{Cell, RefCell},
        pin::pin,
        rc::Rc,
        sync::Arc,
        task::{Context, Poll, Wake, Waker},
    };

    use ntex::codec::BytesCodec;
    use ntex_bytes::Bytes;
    use ntex_io::{Filter, Handle, IoContext, IoRef, IoStream, Readiness, types::HttpProtocol};
    use ntex_service::cfg::SharedCfg;

    use super::*;

    thread_local! {
        static TLS_BORROWS: Cell<usize> = const { Cell::new(0) };
        static ON_WAKE: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::default();
        static CALLBACK_IO: RefCell<Vec<IoRef>> = RefCell::default();
        static CALLBACK_QUERIES: RefCell<Vec<(usize, bool)>> = RefCell::default();
    }

    /// Registers an io for handshake callbacks, which must be `Send + Sync`
    /// and therefore refer to it by the returned index.
    pub(crate) fn callback_io(io: IoRef) -> usize {
        CALLBACK_IO.with_borrow_mut(|ios| {
            ios.push(io);
            ios.len() - 1
        })
    }

    /// Queries a registered io from a handshake callback and records whether
    /// the always-available `HttpProtocol` item was answered.
    pub(crate) fn query_from_callback(idx: usize) {
        let io = CALLBACK_IO.with_borrow(|ios| ios[idx].clone());
        let answered = io.query::<HttpProtocol>().as_ref().is_some();
        CALLBACK_QUERIES.with_borrow_mut(|queries| queries.push((idx, answered)));
    }

    /// Returns the recorded callback queries and clears the registry.
    pub(crate) fn callback_queries() -> Vec<(usize, bool)> {
        CALLBACK_IO.with_borrow_mut(Vec::clear);
        CALLBACK_QUERIES.take()
    }

    /// Tracks TLS reference scopes so a regression fails before reentrant
    /// shutdown could create an aliased mutable reference.
    pub(crate) struct TlsBorrow;

    impl TlsBorrow {
        pub(crate) fn new() -> Self {
            TLS_BORROWS.set(TLS_BORROWS.get() + 1);
            Self
        }
    }

    impl Drop for TlsBorrow {
        fn drop(&mut self) {
            TLS_BORROWS.set(TLS_BORROWS.get() - 1);
        }
    }

    struct InlineWake;

    impl Wake for InlineWake {
        fn wake(self: Arc<Self>) {
            let hook = ON_WAKE.with_borrow_mut(Option::take);
            if let Some(hook) = hook {
                hook();
            }
        }
    }

    struct Transport(Rc<RefCell<Option<IoContext>>>);

    impl IoStream for Transport {
        fn start(self, ctx: IoContext) -> Box<dyn Handle> {
            self.0.replace(Some(ctx));
            Box::new(self)
        }
    }

    impl Handle for Transport {}

    pub(crate) fn io(tag: &'static str) -> (Io, Rc<IoContext>) {
        let slot = Rc::new(RefCell::new(None));
        let io = Io::new(Transport(slot.clone()), SharedCfg::new(tag));
        let ctx = Rc::new(slot.take().unwrap());
        (io, ctx)
    }

    pub(crate) fn transfer(from: &IoContext, to: &IoContext) -> usize {
        let data = from.with_write_dst(|buf| {
            let mut data = Vec::new();
            while let Some(page) = buf.take() {
                data.extend_from_slice(&page);
            }
            data
        });
        if !data.is_empty() {
            from.update_write_status(Ok(data.len()));
            to.with_read_buf(|buf| {
                buf.extend_from_slice(&data);
                Poll::Ready(Ok(data.len()))
            });
        }
        data.len()
    }

    pub(crate) fn handshake<R>(
        future: impl Future<Output = R>,
        client: &IoContext,
        server: &IoContext,
    ) -> R {
        let mut future = pin!(future);
        let mut cx = Context::from_waker(Waker::noop());
        for _ in 0..32 {
            let result = future.as_mut().poll(&mut cx);
            transfer(client, server);
            transfer(server, client);
            if let Poll::Ready(result) = result {
                return result;
            }
        }
        panic!("TLS handshake stalled");
    }

    pub(crate) async fn peer_close<F: Filter, G: Filter>(
        sender: &Io<F>,
        sender_ctx: &IoContext,
        receiver: &Io<G>,
        receiver_ctx: &Rc<IoContext>,
    ) {
        let data = Bytes::from(vec![b'x'; 32 * 1024]);
        sender.encode(data.clone(), &BytesCodec).unwrap();
        receiver
            .encode(Bytes::from_static(b"pending reply"), &BytesCodec)
            .unwrap();
        sender.close();
        let mut cx = Context::from_waker(Waker::noop());
        let _ = sender_ctx.poll_read_ready(&mut cx);

        let waker = Waker::from(Arc::new(InlineWake));
        assert!(matches!(
            receiver_ctx.poll_read_ready(&mut Context::from_waker(&waker)),
            Poll::Ready(Readiness::Ready)
        ));
        let called = Rc::new(Cell::new(false));
        let called2 = called.clone();
        let ctx = receiver_ctx.clone();
        ON_WAKE.with_borrow_mut(|hook| {
            assert!(hook.is_none());
            *hook = Some(Box::new(move || {
                assert_eq!(
                    TLS_BORROWS.get(),
                    0,
                    "shutdown woke with TLS state borrowed"
                );
                let _ = ctx.poll_read_ready(&mut Context::from_waker(Waker::noop()));
                called2.set(true);
            }));
        });
        assert!(transfer(sender_ctx, receiver_ctx) > 0);
        ON_WAKE.with_borrow_mut(Option::take);
        assert!(called.get(), "peer close did not wake the read task");
        assert_eq!(receiver.decode(&BytesCodec).unwrap(), Some(data));

        // Shutdown must flush both queued application data and close_notify.
        assert!(transfer(receiver_ctx, sender_ctx) > 0);
        let _ = sender_ctx.poll_read_ready(&mut cx);
        assert_eq!(
            sender.decode(&BytesCodec).unwrap(),
            Some(Bytes::from_static(b"pending reply"))
        );
        // Let the runtime retire deferred write notifications after the drain.
        ntex_util::time::sleep(Millis(1)).await;
        for ctx in [sender_ctx, receiver_ctx.as_ref()] {
            ctx.update_write_status(Ok(0));
            let _ = ctx.poll_read_ready(&mut cx);
            let ready = ctx.poll_write_ready(&mut cx);
            assert!(
                matches!(ready, Poll::Ready(Readiness::Close)),
                "{}: {ready:?}, {:?}",
                ctx.tag(),
                ctx.flags()
            );
            ctx.stopped(None);
        }
        assert!(matches!(
            sender.poll_status_update(&mut cx),
            Poll::Ready(ntex_io::IoStatusUpdate::PeerGone(None))
        ));
        assert!(matches!(
            receiver.poll_status_update(&mut cx),
            Poll::Ready(ntex_io::IoStatusUpdate::PeerGone(None))
        ));
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, pin::pin, task::Poll};

    use ntex_util::future::lazy;

    use super::*;

    /// STATEMENT: Clean transport EOF succeeds only if the final handshake state is complete.
    ///
    /// HOW:
    /// - Poll a handshake until it waits for input, then report EOF with completed or pending state.
    /// - Check completed negotiation succeeds and incomplete negotiation returns `UnexpectedEof`.
    #[ntex::test]
    async fn handshake_final_state_at_eof() {
        for complete in [true, false] {
            let (io, ctx) = testing::io("TLS");
            let handshaking = Cell::new(true);
            let mut handshake = pin!(handshake(&io, || Ok(handshaking.get())));
            assert!(lazy(|cx| handshake.as_mut().poll(cx)).await.is_pending());

            handshaking.set(!complete);
            ctx.with_read_buf(|_| Poll::Ready(Ok(0)));
            let Poll::Ready(result) = lazy(|cx| handshake.as_mut().poll(cx)).await else {
                panic!("handshake did not finish after EOF");
            };
            if complete {
                result.unwrap();
            } else {
                assert_eq!(result.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
            }
        }
    }

    /// STATEMENT: A handshake error waits for its alert to flush and survives a flush failure.
    ///
    /// HOW:
    /// - Queue alert bytes, report a negotiation error, and check the handshake waits for delivery.
    /// - Complete or fail the transport write and check the original handshake error is returned.
    #[ntex::test]
    async fn handshake_error_flushes_alert() {
        for write_fails in [false, true] {
            let (io, ctx) = testing::io("TLS");
            io.encode_slice(b"alert").unwrap();
            let failed = Cell::new(false);
            let mut handshake = pin!(handshake(&io, || {
                failed.set(true);
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "handshake rejected",
                ))
            }));
            assert!(lazy(|cx| handshake.as_mut().poll(cx)).await.is_pending());
            assert!(failed.get());

            let alert = ctx.with_write_dst(ntex_bytes::BytePages::freeze);
            assert_eq!(alert.as_ref(), b"alert");
            assert!(lazy(|cx| handshake.as_mut().poll(cx)).await.is_pending());
            if write_fails {
                ctx.update_write_status(Err(io::Error::from(io::ErrorKind::BrokenPipe)));
            } else {
                ctx.update_write_status(Ok(alert.len()));
            }
            let Poll::Ready(result) = lazy(|cx| handshake.as_mut().poll(cx)).await else {
                panic!("handshake did not finish after alert write");
            };
            let err = result.unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData);
            assert_eq!(err.to_string(), "handshake rejected");
        }
    }

    /// STATEMENT: The handshake deadline starts on the first poll, not when the future is created.
    ///
    /// HOW:
    /// - Create a 50 ms `with_timeout` future around a pending handshake and sleep 150 ms.
    /// - Check the first poll is still pending, then check awaiting it returns `TimedOut`.
    #[ntex::test]
    async fn with_timeout_starts_on_first_poll() {
        let mut fut = pin!(with_timeout(
            Millis(50),
            std::future::pending::<io::Result<()>>()
        ));
        ntex_util::time::sleep(Millis(150)).await;
        assert!(lazy(|cx| fut.as_mut().poll(cx)).await.is_pending());

        let err = fut.await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }

    /// STATEMENT: Only the `h2` ALPN value selects HTTP/2; other values default to HTTP/1.
    ///
    /// HOW:
    /// - Compare `http_protocol` results for `h2`, `http/1.1`, `h2c`, and no ALPN value.
    #[test]
    fn test_http_protocol() {
        assert_eq!(http_protocol(Some(b"h2")), HttpProtocol::Http2);
        assert_eq!(http_protocol(Some(b"http/1.1")), HttpProtocol::Http1);
        assert_eq!(http_protocol(Some(b"h2c")), HttpProtocol::Http1);
        assert_eq!(http_protocol(None), HttpProtocol::Http1);
    }

    /// STATEMENT: Server names discard ports and IPv6 brackets without damaging host literals.
    ///
    /// HOW:
    /// - Check DNS names, IPv4, bracketed and bare IPv6, a scoped IPv6 address, and empty input.
    /// - Compare each result with the expected host, retaining its IPv6 scope when present.
    #[test]
    fn test_server_name() {
        assert_eq!(server_name("example.com"), "example.com");
        assert_eq!(server_name("example.com:443"), "example.com");
        assert_eq!(server_name("127.0.0.1:8080"), "127.0.0.1");
        assert_eq!(server_name("[::1]"), "::1");
        assert_eq!(server_name("[::1]:443"), "::1");
        assert_eq!(server_name("[fe80::1%25eth0]:443"), "fe80::1%25eth0");
        assert_eq!(server_name("::1"), "::1");
        assert_eq!(server_name("2001:db8::1"), "2001:db8::1");
        assert_eq!(server_name(""), "");
    }
}
