//! Service dispatcher for framed I/O transports.
//!
//! [`Dispatcher`] reads frames from an `ntex-io` transport using an
//! `ntex-codec` decoder and forwards them to an `ntex-service` pipeline as
//! [`DispatchItem`] values. The service may return an encoded response, report
//! a service error, or return `None` when no response is required.
//!
//! The dispatcher also reports write backpressure through [`Control`] messages
//! and delivers disconnect, codec, keep-alive, frame-read, and write
//! failures through [`Reason`] before shutting down the service.
#![deny(clippy::pedantic)]
#![allow(clippy::cast_possible_truncation)]
use std::task::{Context, Poll, ready};
use std::{cell::Cell, fmt, future::Future, io, pin::Pin, rc::Rc};

use ntex_codec::{Decoder, Encoder};
use ntex_io::{Decoded, IoBoxed, IoStatusUpdate, RecvError};
use ntex_service::pipeline::{Pipeline, PipelineCall};
use ntex_util::{spawn, time::Seconds};

mod timer;

use self::timer::{Timer, Timers};

type Response<U> = <U as Encoder>::Item;

/// Event delivered to the dispatcher service.
pub enum DispatchItem<U: Encoder + Decoder> {
    /// A frame decoded from the transport.
    Item(<U as Decoder>::Item),
    /// A transport flow-control notification.
    Control(Control),
    /// The dispatcher is stopping for the specified reason.
    Stop(Reason<U>),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
/// Write-side flow-control notification.
pub enum Control {
    /// Write backpressure has been enabled.
    WBackPressureEnabled,
    /// Write backpressure has been disabled.
    WBackPressureDisabled,
}

/// Reason a dispatcher is stopping.
pub enum Reason<U: Encoder + Decoder> {
    /// Service error
    Service,
    /// The transport disconnected.
    ///
    /// The value contains the underlying I/O error when one was available.
    /// If the peer closed its side cleanly while undecodable bytes were left
    /// in the read buffer, the stream was truncated and the value contains an
    /// [`io::ErrorKind::UnexpectedEof`] error.
    Io(Option<io::Error>),
    /// A service response could not be encoded.
    Encoder(<U as Encoder>::Error),
    /// Incoming bytes could not be decoded.
    Decoder(<U as Decoder>::Error),
    /// The connection exceeded its keep-alive timeout.
    KeepAlive,
    /// A complete frame was not received within the configured read deadline.
    ReadTimeout,
    /// Write backpressure stayed enabled for longer than the configured
    /// write timeout.
    WriteTimeout,
}

/// Reports a truncated stream, the peer closed cleanly in the middle of a frame.
fn truncated(io: &IoBoxed) -> Option<io::Error> {
    if io.is_read_eof() && io.with_read_dst(|buf| !buf.is_empty()) {
        Some(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "bytes remaining on stream",
        ))
    } else {
        None
    }
}

/// Future that dispatches decoded transport frames to a service.
///
/// The service receives [`DispatchItem`] values and returns
/// `Option<U::Item>`, where `Some(item)` is encoded and written to the
/// transport and `None` produces no response.
///
/// Multiple service calls may be in flight concurrently. When the
/// transport applies write backpressure, the dispatcher pauses normal
/// reads and emits [`Control::WBackPressureEnabled`]. It emits
/// [`Control::WBackPressureDisabled`] before resuming normal processing.
///
/// Before shutdown, transport and codec failures are delivered to the
/// service as [`DispatchItem::Stop`]. The future resolves to `Err` only
/// when the service itself fails. Graceful and protocol stops drain service
/// calls before shutdown; transport failures abandon pending calls after
/// delivering the stop notification so they cannot block teardown.
pub struct Dispatcher<U, Err>
where
    U: Encoder + Decoder + 'static,
    Err: 'static,
{
    inner: DispatcherInner<U, Err>,
}

// The dispatcher never pins its fields, all futures it polls are `Unpin`.
impl<U: Encoder + Decoder, Err> Unpin for Dispatcher<U, Err> {}

impl<U: Encoder + Decoder, Err> fmt::Debug for Dispatcher<U, Err> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Dispatcher").finish_non_exhaustive()
    }
}

type Call<U, Err> = PipelineCall<DispatchItem<U>, Option<Response<U>>, Err>;
type Service<U, Err> = Pipeline<DispatchItem<U>, Option<Response<U>>, Err>;

struct DispatcherInner<U, Err>
where
    U: Encoder + Decoder + 'static,
{
    st: DispatcherState<U, Err>,
    error: Option<Err>,
    shared: Rc<DispatcherShared<U, Err>>,
    response: Option<Call<U, Err>>,
    timers: Timers,
}

pub(crate) struct DispatcherShared<U, Err>
where
    U: Encoder + Decoder,
{
    io: IoBoxed,
    codec: U,
    service: Service<U, Err>,
    keepalive: bool,
    error: Cell<Option<DispatcherError<Err, <U as Encoder>::Error>>>,
    inflight: Cell<u32>,
}

#[derive(Debug)]
enum DispatcherState<U: Encoder + Decoder, Err> {
    Processing,
    Backpressure,
    Stop(Call<U, Err>),
    Shutdown,
    ShutdownIo,
}

#[derive(Debug)]
enum DispatcherError<S, U> {
    Encoder(U),
    Service(S),
}

enum PollService<U: Encoder + Decoder> {
    Item(DispatchItem<U>),
    Continue,
    Ready,
}

impl<U, Err> Dispatcher<U, Err>
where
    U: Decoder + Encoder + 'static,
    Err: 'static,
{
    /// Creates a dispatcher for an I/O transport, codec, and service pipeline.
    ///
    /// Keep-alive and frame-read timeout behavior is taken from the transport's
    /// `ntex_io::IoConfig`.
    pub fn new<Io>(io: Io, codec: U, service: Service<U, Err>) -> Dispatcher<U, Err>
    where
        IoBoxed: From<Io>,
    {
        let io = IoBoxed::from(io);
        let shared = Rc::new(DispatcherShared {
            keepalive: !io.cfg().keepalive_timeout().is_zero(),
            io,
            codec,
            service,
            error: Cell::new(None),
            inflight: Cell::new(0),
        });

        Dispatcher {
            inner: DispatcherInner {
                timers: Timers::new(&shared.io),
                shared,
                response: None,
                error: None,
                st: DispatcherState::Processing,
            },
        }
    }
}

impl<U, Err> DispatcherShared<U, Err>
where
    U: Encoder + Decoder + 'static,
    Err: 'static,
{
    fn call(&self, item: DispatchItem<U>, nowait: bool) -> Call<U, Err> {
        self.inflight.set(self.inflight.get() + 1);
        if nowait {
            self.service.call_nowait(item)
        } else {
            self.service.call_static(item)
        }
    }

    fn handle_result(&self, item: Result<Option<Response<U>>, Err>, wake: bool) {
        match item {
            Ok(Some(val)) => {
                if let Err(err) = self.io.encode(val, &self.codec) {
                    self.error.set(Some(DispatcherError::Encoder(err)));
                }
            }
            Err(err) => self.error.set(Some(DispatcherError::Service(err))),
            Ok(None) => (),
        }
        self.inflight.set(self.inflight.get() - 1);
        if wake {
            self.io.notify_dispatcher();
        }
    }
}

impl<U, Err> Future for Dispatcher<U, Err>
where
    U: Decoder + Encoder + 'static,
    Err: 'static,
{
    type Output = Result<(), Err>;

    #[allow(clippy::too_many_lines)]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let inner = &mut self.get_mut().inner;

        // handle service response future
        if let Some(fut) = inner.response.as_mut()
            && let Poll::Ready(item) = Pin::new(fut).poll(cx)
        {
            inner.shared.handle_result(item, false);
            inner.response = None;
        }

        loop {
            match inner.st {
                DispatcherState::Processing => {
                    let (item, nowait) = match ready!(inner.poll_service(cx)) {
                        PollService::Ready => {
                            // decode incoming bytes if buffer is ready
                            match inner.shared.io.poll_recv_decode(&inner.shared.codec, cx) {
                                Ok(decoded) => {
                                    inner.update_timer(&decoded);
                                    if let Some(el) = decoded.item {
                                        (DispatchItem::Item(el), true)
                                    } else {
                                        return Poll::Pending;
                                    }
                                }
                                Err(RecvError::Timeout) => {
                                    if let Err(ctl) = inner.handle_timeout() {
                                        inner.st = inner.stop(ctl);
                                    }
                                    continue;
                                }
                                Err(RecvError::WriteBackpressure) => {
                                    // instruct write task to notify dispatcher when data is flushed
                                    inner.start_write_timer();
                                    inner.st = DispatcherState::Backpressure;
                                    (DispatchItem::Control(Control::WBackPressureEnabled), true)
                                }
                                Err(RecvError::Decoder(err)) => {
                                    log::trace!(
                                        "{}: Decoder error, stopping dispatcher: {:?}",
                                        inner.shared.io.tag(),
                                        err
                                    );
                                    inner.st = inner.stop(Reason::Decoder(err));
                                    continue;
                                }
                                Err(RecvError::PeerGone(err)) => {
                                    let err = err.or_else(|| truncated(&inner.shared.io));
                                    log::trace!(
                                        "{}: Peer is gone, stopping dispatcher: {:?}",
                                        inner.shared.io.tag(),
                                        err
                                    );
                                    inner.st = inner.stop(Reason::Io(err));
                                    continue;
                                }
                            }
                        }
                        PollService::Item(item) => (item, false),
                        PollService::Continue => continue,
                    };

                    inner.call_service(cx, item, nowait);
                }
                // handle write back-pressure
                DispatcherState::Backpressure => {
                    match ready!(inner.poll_service(cx)) {
                        PollService::Ready
                        | PollService::Item(DispatchItem::Control(Control::WBackPressureEnabled)) =>
                            {}
                        PollService::Item(item) => inner.call_service(cx, item, false),
                        PollService::Continue => continue,
                    }

                    // check write timeout
                    if let Poll::Ready(IoStatusUpdate::Timeout) =
                        inner.shared.io.poll_status_update(cx)
                        && let Err(reason) = inner.handle_timeout()
                    {
                        inner.st = inner.stop(reason);
                        continue;
                    }

                    let item = if let Err(err) = ready!(inner.shared.io.poll_flush(cx, false)) {
                        inner.st = inner.stop(Reason::Io(Some(err)));
                        continue;
                    } else {
                        // Stops the write timeout when write backpressure is disabled.
                        inner.stop_timer();
                        inner.st = DispatcherState::Processing;
                        DispatchItem::Control(Control::WBackPressureDisabled)
                    };
                    inner.call_service(cx, item, false);
                }
                // deliver stop to service
                DispatcherState::Stop(ref mut stop) => {
                    // service may relay on poll_ready for response results
                    let _ = inner.shared.service.poll_ready(cx);

                    let result = ready!(Pin::new(stop).poll(cx));
                    inner.shared.handle_result(result, false);
                    // the dispatcher returns a service error from the stop call,
                    // unless it is stopping because of an earlier one
                    if let Some(DispatcherError::Service(err)) = inner.shared.error.take()
                        && inner.error.is_none()
                    {
                        inner.error = Some(err);
                    }
                    inner.shared.io.stop_timer();
                    inner.st = DispatcherState::Shutdown;
                }
                // drain service responses and shutdown service
                DispatcherState::Shutdown => {
                    // service may relay on poll_ready for response results
                    let _ = inner.shared.service.poll_ready(cx);

                    if inner.shared.inflight.get() != 0 {
                        inner.shared.io.register_dispatch(cx);
                        return Poll::Pending;
                    }

                    ready!(inner.shared.service.poll_shutdown(cx));
                    log::trace!(
                        "{}: Service shutdown is completed, stop",
                        inner.shared.io.tag()
                    );
                    inner.st = DispatcherState::ShutdownIo;
                }
                // shutdown io
                DispatcherState::ShutdownIo => {
                    let _ = ready!(inner.shared.io.poll_shutdown(cx));

                    return Poll::Ready(if let Some(err) = inner.error.take() {
                        Err(err)
                    } else {
                        Ok(())
                    });
                }
            }
        }
    }
}

impl<U, Err> DispatcherInner<U, Err>
where
    U: Decoder + Encoder + 'static,
    Err: 'static,
{
    fn stop(&self, reason: Reason<U>) -> DispatcherState<U, Err> {
        DispatcherState::Stop(self.shared.call(DispatchItem::Stop(reason), true))
    }

    fn call_service(&mut self, cx: &mut Context<'_>, item: DispatchItem<U>, nowait: bool) {
        let mut fut = self.shared.call(item, nowait);

        // optimize first call
        if self.response.is_none() {
            if let Poll::Ready(result) = Pin::new(&mut fut).poll(cx) {
                self.shared.handle_result(result, false);
            } else {
                self.response = Some(fut);
            }
        } else {
            let shared = self.shared.clone();
            spawn(async move {
                let result = fut.await;
                shared.handle_result(result, true);
            });
        }
    }

    fn check_error(&mut self) -> PollService<U> {
        // check for errors
        if let Some(err) = self.shared.error.take() {
            log::trace!(
                "{}: Error occurred, stopping dispatcher",
                self.shared.io.tag()
            );
            match err {
                DispatcherError::Encoder(err) => {
                    self.st = self.stop(Reason::Encoder(err));
                }
                DispatcherError::Service(err) => {
                    self.error = Some(err);
                    self.st = self.stop(Reason::Service);
                }
            }
            PollService::Continue
        } else {
            PollService::Ready
        }
    }

    fn poll_service(&mut self, cx: &mut Context<'_>) -> Poll<PollService<U>> {
        // wait until service becomes ready
        match self.shared.service.poll_ready(cx) {
            Poll::Ready(Ok(())) => Poll::Ready(self.check_error()),
            // pause io read task
            Poll::Pending => {
                log::trace!(
                    "{}: Service is not ready, register dispatcher",
                    self.shared.io.tag()
                );

                // the write timeout keeps running while the service is paused
                if self.timers.active != Timer::Write {
                    self.stop_timer();
                }
                self.timers.reset_read(self.shared.io.cfg());

                match ready!(self.shared.io.poll_read_pause(cx)) {
                    IoStatusUpdate::Timeout => {
                        if let Err(reason) = self.handle_timeout() {
                            log::trace!(
                                "{}: Timeout during pause, stopping dispatcher: {:?}",
                                self.shared.io.tag(),
                                reason
                            );
                            self.st = self.stop(reason);
                        }
                        Poll::Ready(PollService::Continue)
                    }
                    IoStatusUpdate::PeerGone(err) => {
                        log::trace!(
                            "{}: Peer is gone during pause, stopping dispatcher: {:?}",
                            self.shared.io.tag(),
                            err
                        );
                        self.st = self.stop(Reason::Io(err));
                        Poll::Ready(PollService::Continue)
                    }
                    IoStatusUpdate::WriteBackpressure => {
                        if !matches!(self.st, DispatcherState::Backpressure) {
                            self.start_write_timer();
                        }
                        self.st = DispatcherState::Backpressure;
                        Poll::Ready(PollService::Item(DispatchItem::Control(
                            Control::WBackPressureEnabled,
                        )))
                    }
                }
            }
            // handle service readiness error
            Poll::Ready(Err(err)) => {
                log::trace!(
                    "{}: Service readiness check failed, stopping",
                    self.shared.io.tag()
                );
                self.st = self.stop(Reason::Service);
                self.error = Some(err);
                Poll::Ready(PollService::Continue)
            }
        }
    }

    /// Starts the write timeout when write backpressure is enabled.
    ///
    /// Frames are not decoded during backpressure, so read-side timers are
    /// stopped when no write timeout is configured.
    fn start_write_timer(&mut self) {
        let timeout = self.shared.io.cfg().write_timeout();
        if timeout.is_zero() {
            self.stop_timer();
        } else if self.timers.active != Timer::Write {
            self.timers.active = Timer::Write;
            self.shared.io.start_timer(timeout);
        }
    }

    fn update_timer(&mut self, decoded: &Decoded<<U as Decoder>::Item>) {
        let item = decoded.item.is_some();
        self.timers.update_read(
            self.shared.io.cfg(),
            item,
            decoded.remains as u32,
            decoded.consumed as u32,
        );

        // keep-alive and frame read timers do not apply while a frame is handled
        let handling = item || self.shared.inflight.get() != 0;
        let timer = self
            .timers
            .select(self.shared.io.cfg(), self.shared.keepalive, handling);
        self.set_timer(timer);
    }

    /// Stops the dispatcher timer, if it is armed.
    fn stop_timer(&mut self) {
        if self.timers.active != Timer::Stopped {
            self.timers.active = Timer::Stopped;
            self.shared.io.stop_timer();
        }
    }

    /// Arms the dispatcher timer for a read-side purpose, an armed timer
    /// with the same purpose keeps running.
    fn set_timer(&mut self, timer: Timer) {
        if self.timers.active == timer {
            return;
        }
        let io = &self.shared.io;
        self.timers.active = match timer {
            Timer::KeepAlive => {
                log::trace!(
                    "{}: Start keep-alive timer {:?}",
                    io.tag(),
                    io.cfg().keepalive_timeout()
                );
                io.start_timer(io.cfg().keepalive_timeout());
                Timer::KeepAlive
            }
            Timer::FrameRead if let Some(params) = io.cfg().frame_read_rate() => {
                io.start_timer(params.timeout);
                Timer::FrameRead
            }
            _ => {
                io.stop_timer();
                Timer::Stopped
            }
        };
    }

    fn handle_timeout(&mut self) -> Result<(), Reason<U>> {
        match self.timers.active {
            Timer::FrameRead => {
                let (Some(params), Some(p)) = (
                    self.shared.io.cfg().frame_read_rate(),
                    self.timers.read.progress(),
                ) else {
                    self.timers.active = Timer::Stopped;
                    return Ok(());
                };

                // read rate, start timer for next period
                if p.consumed > params.rate {
                    let total = p.consumed;
                    p.consumed = 0;

                    if !params.max_timeout.is_zero() {
                        p.max_timeout = Seconds(p.max_timeout.0.saturating_sub(params.timeout.0));
                    }

                    if params.max_timeout.is_zero() || !p.max_timeout.is_zero() {
                        log::trace!(
                            "{}: Frame read rate {:?}, extend timer",
                            self.shared.io.tag(),
                            total
                        );
                        self.shared.io.start_timer(params.timeout);
                        return Ok(());
                    }
                    log::trace!(
                        "{}: Max payload timeout has been reached",
                        self.shared.io.tag()
                    );
                }
                Err(Reason::ReadTimeout)
            }
            // backpressure can be released unnoticed while the service is paused
            Timer::Write if !self.shared.io.is_wr_backpressure() => {
                self.timers.active = Timer::Stopped;
                Ok(())
            }
            Timer::Write => {
                log::trace!(
                    "{}: Write backpressure timeout, stopping dispatcher",
                    self.shared.io.tag()
                );
                Err(Reason::WriteTimeout)
            }
            Timer::KeepAlive => {
                log::trace!(
                    "{}: Keep-alive error, stopping dispatcher",
                    self.shared.io.tag()
                );
                Err(Reason::KeepAlive)
            }
            // external timeout, applies to idle connection
            Timer::Stopped if self.shared.inflight.get() == 0 => {
                log::trace!(
                    "{}: Idle timeout, stopping dispatcher",
                    self.shared.io.tag()
                );
                Err(Reason::KeepAlive)
            }
            Timer::Stopped => Ok(()),
        }
    }
}

impl<U> fmt::Debug for DispatchItem<U>
where
    U: Encoder + Decoder,
    <U as Decoder>::Item: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DispatchItem::Item(item) => f.debug_tuple("DispatchItem::Item").field(item).finish(),
            DispatchItem::Control(e) => f.debug_tuple("DispatchItem::Control").field(e).finish(),
            DispatchItem::Stop(e) => f.debug_tuple("DispatchItem::Stop").field(e).finish(),
        }
    }
}

impl<U> fmt::Debug for Reason<U>
where
    U: Encoder + Decoder,
    <U as Encoder>::Error: fmt::Debug,
    <U as Decoder>::Error: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Reason::Service => f.write_str("Reason::Service"),
            Reason::Io(err) => f.debug_tuple("Reason::Io").field(err).finish(),
            Reason::Encoder(err) => f.debug_tuple("Reason::Encoder").field(err).finish(),
            Reason::Decoder(err) => f.debug_tuple("Reason::Decoder").field(err).finish(),
            Reason::KeepAlive => f.write_str("Reason::KeepAlive"),
            Reason::ReadTimeout => f.write_str("Reason::ReadTimeout"),
            Reason::WriteTimeout => f.write_str("Reason::WriteTimeout"),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unused_async_trait_impl)]
mod tests {
    use std::sync::{Arc, Mutex, atomic::AtomicBool, atomic::Ordering::Relaxed};
    use std::{cell::RefCell, future::poll_fn, io};

    use ntex_bytes::{BytePages, Bytes, BytesMut};
    use ntex_codec::BytesCodec;
    use ntex_io::{Io, IoConfig, IoRef, testing::IoTest};
    use ntex_service::{Ctx, Pipeline, Service, cfg::SharedCfg};
    use ntex_util::time::{Millis, sleep, timeout};
    use ntex_util::{channel::oneshot, future::lazy};
    use rand::Rng;

    use super::*;

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
        sleep(Millis(1500)).await;
        assert!(client.is_server_dropped());
    }

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
        sleep(Millis(250)).await;
        assert!(client.is_closed());

        // close read side
        client.close().await;

        // dispatcher is closed
        assert!(client.is_server_dropped());
    }

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
        sleep(Millis(250)).await;
        assert!(client.is_closed());

        // close read side
        client.close().await;
        assert!(client.is_server_dropped());

        // service must be checked for readiness all the time
        assert_eq!(counter.get(), 3);
    }

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
                    .set_read_buf(8 * 1024, 1024, 16)
                    .set_write_buf(16 * 1024),
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
                        .set_read_buf(1024, 512, 16),
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
            .take(1024)
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

    #[ntex::test]
    async fn application_close_waits_for_pending_service_call() {
        let started = Arc::new(AtomicBool::new(false));
        let started2 = started.clone();
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);

        let (mut disp, state) = Dispatcher::debug(
            Io::from(server),
            BytesCodec,
            ntex_service::fn_service(move |msg: DispatchItem<BytesCodec>| {
                let started = started2.clone();
                async move {
                    if matches!(msg, DispatchItem::Item(_)) {
                        started.store(true, Relaxed);
                        sleep(Millis(200)).await;
                    }
                    Ok::<_, ()>(None)
                }
            }),
        );

        client.write("request");
        sleep(Millis(25)).await;
        assert!(lazy(|cx| Pin::new(&mut disp).poll(cx)).await.is_pending());
        assert!(started.load(Relaxed));

        state.close();
        assert!(
            timeout(Millis(50), poll_fn(|cx| Pin::new(&mut disp).poll(cx)))
                .await
                .is_err()
        );
        timeout(Millis(1000), poll_fn(|cx| Pin::new(&mut disp).poll(cx)))
            .await
            .expect("dispatcher did not drain pending service call")
            .unwrap();
    }

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
        sleep(Millis(2000)).await;

        // write side must be closed, dispatcher should fail with keep-alive
        assert!(!state.0.is_active());
        assert!(client.is_closed());
        assert_eq!(&data.lock().unwrap().borrow()[..], &[0, 1]);
    }

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
        sleep(Millis(2000)).await;

        // write side must be closed, dispatcher should fail with keep-alive
        assert!(!state.0.is_active());
        assert!(client.is_closed());
        assert_eq!(&data.lock().unwrap().borrow()[..], &[0, 1]);
    }

    /// Update keep-alive timer after receiving frame
    #[ntex::test]
    async fn keepalive3() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);

        let data = Arc::new(Mutex::new(RefCell::new(Vec::new())));
        let data2 = data.clone();

        let cfg = SharedCfg::new("DBG").add(
            IoConfig::new()
                .set_keepalive_timeout(Seconds(2))
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
        sleep(Millis(750)).await;

        client.write("2");
        let buf = client.read().await.unwrap();
        assert_eq!(buf, Bytes::from_static(b"2"));

        sleep(Millis(750)).await;
        client.write("3");
        let buf = client.read().await.unwrap();
        assert_eq!(buf, Bytes::from_static(b"3"));

        sleep(Millis(750)).await;
        assert!(!client.is_closed());
        assert_eq!(&data.lock().unwrap().borrow()[..], &[0, 0, 0]);
    }

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

        client.write("1");
        sleep(Millis(1000)).await;
        assert!(state.0.is_active());
        client.write("23");
        sleep(Millis(1000)).await;
        assert!(state.0.is_active());
        client.write("4");
        sleep(Millis(2000)).await;

        // write side must be closed, dispatcher should fail with keep-alive
        assert!(!state.0.is_active());
        assert!(client.is_closed());
        assert_eq!(&data.lock().unwrap().borrow()[..], &[0, 1]);
    }

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

        sleep(Millis(1000)).await;
        assert!(!state.0.is_active());
        assert!(client.is_closed());
    }

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

        let (disp, _) = Dispatcher::debug(
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

        reason.borrow_mut().take().expect("dispatcher did not stop")
    }

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

    #[ntex::test]
    async fn peer_eof_reports_truncated_frame() {
        assert_eq!(
            stop_reason_at_eof("123").await,
            Some(io::ErrorKind::UnexpectedEof)
        );
    }

    #[ntex::test]
    async fn peer_eof_after_whole_frame_is_clean() {
        assert_eq!(stop_reason_at_eof("12345678").await, None);
    }

    /// Service becomes not ready and write backpressure is enabled
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
                SharedCfg::new("DBG").add(IoConfig::new().set_write_buf(2)),
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

    /// A completed frame stops the read timer, so it cannot close an idle
    /// connection when keep-alive is disabled.
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

    /// Bytes consumed by the codec without producing a frame count as read
    /// progress.
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
        for _ in 0..7 {
            sleep(Millis(700)).await;
            client.write("abc#");
        }
        assert!(!client.is_closed());
        assert!(data.lock().unwrap().borrow().is_empty());

        // no progress, the frame read timer expires
        sleep(Millis(4500)).await;
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

    /// Frame read-rate tracking starts when the connection arrives, so a
    /// silent peer is closed before the keep-alive timeout.
    #[ntex::test]
    async fn first_frame_read_rate_silent_peer() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);

        let data = Arc::new(Mutex::new(RefCell::new(Vec::new())));
        let disp = first_frame_dispatcher(server, data.clone());
        spawn(async move {
            let _ = disp.await;
        });

        sleep(Millis(4500)).await;
        assert!(client.is_closed());
        assert_eq!(&data.lock().unwrap().borrow()[..], &[1]);
    }

    /// Completing the first frame stops frame read-rate tracking, and the
    /// keep-alive timer governs the idle connection.
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

        sleep(Millis(4500)).await;
        assert!(!client.is_closed());
        assert_eq!(&data.lock().unwrap().borrow()[..], &[0]);
    }

    /// Service whose readiness waits on a gate installed by the test.
    struct GateSrv(Rc<RefCell<Option<oneshot::Receiver<()>>>>, Rc<Cell<bool>>);

    impl Service<(), DispatchItem<BCodec>> for GateSrv {
        type Res = Option<Bytes>;
        type Error = ();

        async fn ready(&self, _: Ctx<'_, Self>) -> Result<(), Self::Error> {
            let rx = self.0.borrow_mut().take();
            if let Some(rx) = rx {
                let _ = rx.await;
            }
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

    /// A service pause restarts frame read-rate tracking with a fresh budget,
    /// repeated pauses do not exhaust it.
    #[ntex::test]
    async fn read_rate_budget_reset_on_service_pause() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);

        let gate = Rc::new(RefCell::new(None));
        let timed_out = Rc::new(Cell::new(false));
        let io = Io::new(
            server,
            SharedCfg::new("TEST").add(
                IoConfig::new()
                    .set_keepalive_timeout(Seconds::ZERO)
                    .set_frame_read_rate(Seconds(1), Seconds(3), 2),
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

        // each cycle lets the frame timer extend the period twice, the
        // budget allows two extensions, so it must be restored by the pause
        client.write("abc");
        for _ in 0..5 {
            for _ in 0..3 {
                sleep(Millis(700)).await;
                client.write("abc");
            }
            let (tx, rx) = oneshot::channel();
            *gate.borrow_mut() = Some(rx);
            client.write("abc");
            sleep(Millis(100)).await;
            let _ = tx.send(());
        }
        assert!(!client.is_closed());
        assert!(!timed_out.get());
    }

    /// Frame read-rate tracking restarts after a service pause, a peer that
    /// stops sending is still closed.
    #[ntex::test]
    async fn read_rate_restarts_after_service_pause() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);

        let gate = Rc::new(RefCell::new(None));
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
        *gate.borrow_mut() = Some(rx);
        client.write("abc");
        sleep(Millis(1500)).await;
        let _ = tx.send(());

        // no more data after the pause, the data sent during the pause
        // extends the first period
        sleep(Millis(1500)).await;
        assert!(!client.is_closed());
        sleep(Millis(3500)).await;
        assert!(client.is_closed());
        assert!(timed_out.get());
    }

    /// Frame read-rate tracking starts when the codec consumes partial frame
    /// data into its own state and leaves nothing buffered.
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
        sleep(Millis(4500)).await;
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
                sleep(Millis(1500)).await;
                self.0.set(false);
                return Ok(Some(bytes));
            }
            Ok(None)
        }
    }

    /// An external timeout does not stop the dispatcher while the service is
    /// paused and a frame is handled.
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
        sleep(Millis(1000)).await;
        assert!(!client.is_closed());
        let buf = client.read().await.unwrap();
        assert_eq!(buf, Bytes::from_static(b"1"));
        sleep(Millis(2500)).await;
        assert!(!client.is_closed());
    }

    /// An external timeout stops an idle dispatcher while the service is
    /// paused, the same as when it is ready.
    #[ntex::test]
    async fn notify_timeout_during_idle_pause() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);

        let gate = Rc::new(RefCell::new(None));
        let io = Io::new(
            server,
            SharedCfg::new("TEST").add(IoConfig::new().set_keepalive_timeout(Seconds::ZERO)),
        );
        let ioref = io.get_ref();
        let (tx, rx) = oneshot::channel();
        *gate.borrow_mut() = Some(rx);
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
        sleep(Millis(500)).await;
        assert!(client.is_closed());
    }

    fn keepalive_dispatcher(
        server: IoTest,
        delay: Millis,
        data: Arc<Mutex<RefCell<Vec<usize>>>>,
    ) -> Dispatcher<BCodec, ()> {
        let io = Io::new(
            server,
            SharedCfg::new("TEST").add(IoConfig::new().set_keepalive_timeout(Seconds(1))),
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

    /// The keep-alive timer is not active while a frame is handled, it starts
    /// once the response is done.
    #[ntex::test]
    async fn keepalive_inactive_during_frame_handling() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);

        let data = Arc::new(Mutex::new(RefCell::new(Vec::new())));
        let disp = keepalive_dispatcher(server, Millis(2500), data.clone());
        spawn(async move {
            let _ = disp.await;
        });

        client.write("12345678");
        sleep(Millis(3000)).await;
        assert!(!client.is_closed());
        assert_eq!(&data.lock().unwrap().borrow()[..], &[0]);

        sleep(Millis(3000)).await;
        assert!(client.is_closed());
        assert_eq!(&data.lock().unwrap().borrow()[..], &[0, 1]);
    }

    /// The keep-alive timer is not active while a partial frame is read.
    #[ntex::test]
    async fn keepalive_inactive_during_frame_read() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);

        let data = Arc::new(Mutex::new(RefCell::new(Vec::new())));
        let disp = keepalive_dispatcher(server, Millis(0), data.clone());
        spawn(async move {
            let _ = disp.await;
        });

        // keep-alive is armed for the idle connection, then a frame starts
        sleep(Millis(200)).await;
        client.write("1234");
        sleep(Millis(3500)).await;
        assert!(!client.is_closed());

        client.write("5678");
        let buf = client.read().await.unwrap();
        assert_eq!(buf, Bytes::from_static(b"12345678"));

        sleep(Millis(3000)).await;
        assert!(client.is_closed());
        assert_eq!(&data.lock().unwrap().borrow()[..], &[0, 1]);
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

        async fn ready(&self, _: Ctx<'_, Self>) -> Result<(), Self::Error> {
            let rx = self.gate.borrow_mut().take();
            if let Some(rx) = rx {
                let _ = rx.await;
            }
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

    type Gate = Rc<RefCell<Option<oneshot::Receiver<()>>>>;
    type Events = Rc<RefCell<Vec<&'static str>>>;

    fn write_dispatcher(
        server: IoTest,
        cfg: IoConfig,
        size: usize,
    ) -> (Dispatcher<BCodec, ()>, Gate, Events) {
        let gate = Rc::new(RefCell::new(None));
        let events = Rc::new(RefCell::new(Vec::new()));
        let io = Io::new(server, SharedCfg::new("TEST").add(cfg.set_write_buf(1024)));
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

    /// A peer that stops reading during write backpressure is closed with a
    /// write timeout.
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
        sleep(Millis(4000)).await;
        assert!(client.is_closed());
        assert_eq!(&events.borrow()[..], &["item", "bp-on", "write-timeout"]);
    }

    /// The write timeout covers the whole backpressure period, a peer that
    /// keeps reading too slowly to release backpressure is closed.
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
        }
        assert!(client.is_closed());
        assert_eq!(&events.borrow()[..], &["item", "bp-on", "write-timeout"]);
    }

    /// The write timeout keeps running while the service is not ready during
    /// write backpressure, and the stop item is delivered without waiting for
    /// readiness.
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
        *gate.borrow_mut() = Some(rx);
        sleep(Millis(2500)).await;
        assert_eq!(&events.borrow()[..], &["item", "bp-on", "write-timeout"]);

        // shutdown does not wait for readiness and drains output within
        // the shutdown timeout
        sleep(Millis(2500)).await;
        assert!(client.is_closed());
        assert_eq!(&events.borrow()[..], &["item", "bp-on", "write-timeout"]);
        drop(tx);
    }

    /// Backpressure released while the service is not ready ends the write
    /// timeout, even though the dispatcher observes the release later.
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
        *gate.borrow_mut() = Some(rx);
        client.remote_buffer_cap(65536);
        sleep(Millis(100)).await;
        assert_eq!(client.read_any().len(), 8192);
        sleep(Millis(3500)).await;

        drop(tx);
        sleep(Millis(100)).await;
        assert!(!client.is_closed());
        assert_eq!(&events.borrow()[..], &["item", "bp-on", "bp-off"]);
    }

    /// Keep-alive starts again once write backpressure is released.
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
        sleep(Millis(500)).await;
        client.remote_buffer_cap(65536);
        sleep(Millis(100)).await;
        assert_eq!(client.read_any().len(), 8192);
        assert_eq!(&events.borrow()[..], &["item", "bp-on", "bp-off"]);

        sleep(Millis(4000)).await;
        assert!(client.is_closed());
        assert_eq!(
            &events.borrow()[..],
            &["item", "bp-on", "bp-off", "keepalive"]
        );
    }

    /// The write timeout is stopped once write backpressure is released, even
    /// though output is still outstanding.
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

        sleep(Millis(4500)).await;
        assert!(!client.is_closed());
        assert_eq!(&events.borrow()[..], &["item", "bp-on", "bp-off"]);
    }

    /// Each backpressure period starts a fresh write timeout.
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
        for _ in 0..3 {
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
                "item", "bp-on", "bp-off", "item", "bp-on", "bp-off", "item", "bp-on", "bp-off",
                "item", "bp-on"
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

    /// Without a write timeout, the frame read timer does not run during
    /// write backpressure, while no frames are decoded.
    #[ntex::test]
    async fn read_rate_stopped_during_write_backpressure() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(0);

        let events: Events = Rc::new(RefCell::new(Vec::new()));
        let io = Io::new(
            server,
            SharedCfg::new("TEST").add(IoConfig::new().set_write_buf(1024).set_frame_read_rate(
                Seconds(1),
                Seconds::ZERO,
                0,
            )),
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

        // the peer keeps sending partial frame bytes
        for _ in 0..10 {
            sleep(Millis(500)).await;
            client.write("1");
        }
        assert_eq!(&events.borrow()[..], &["item", "bp-on"]);
    }
}
