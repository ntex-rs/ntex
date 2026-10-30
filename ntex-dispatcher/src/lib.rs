//! Service dispatcher for framed I/O transports.
//!
//! [`Dispatcher`] combines an [`ntex_io`] transport, a codec implementing
//! [`Decoder`] and [`Encoder`], and a [`Pipeline`]. Decoded frames reach the
//! service as [`DispatchItem::Item`]. A successful service call returns either
//! `Some(response)`, which the codec encodes into the transport's write buffer,
//! or `None`, which produces no output.
//!
//! The same service handles write-backpressure [`Control`] events and
//! [`DispatchItem::Stop`] notifications carrying a [`Reason`]. Transport,
//! codec, and timeout failures are reported through these events rather than
//! the dispatcher's `Result` error type; only service errors can be returned
//! as `Err`.
//!
//! # Example
//!
//! An echo service can ignore control and stop events while returning each
//! decoded byte buffer as its response. Run this future on the runtime owning
//! the transport.
//!
//! ```
//! use std::convert::Infallible;
//!
//! use ntex_codec::BytesCodec;
//! use ntex_dispatcher::{DispatchItem, Dispatcher};
//! use ntex_io::IoBoxed;
//! use ntex_service::{Pipeline, fn_service};
//!
//! async fn echo(io: IoBoxed) -> Result<(), Infallible> {
//!     let service = fn_service(async |event: DispatchItem<BytesCodec>| {
//!         let response = match event {
//!             DispatchItem::Item(bytes) => Some(bytes),
//!             DispatchItem::Control(_) | DispatchItem::Stop(_) => None,
//!         };
//!         Ok::<_, Infallible>(response)
//!     });
//!
//!     Dispatcher::new(io, BytesCodec, Pipeline::new((), service))
//!         .max_inflight(16)
//!         .await
//! }
//! ```
//!
//! # Configuration
//!
//! Configure the transport with [`ntex_io::IoConfig`] before constructing the
//! dispatcher:
//!
//! - [`set_keepalive_timeout`](ntex_io::IoConfig::set_keepalive_timeout)
//!   bounds idle periods, not partial-frame reads or active service calls.
//! - [`set_frame_read_rate`](ntex_io::IoConfig::set_frame_read_rate) bounds
//!   progress on an incomplete frame, including the first frame before any
//!   bytes arrive. Bytes consumed by the codec without producing a frame
//!   still count as progress.
//! - [`set_write_backpressure`](ntex_io::IoConfig::set_write_backpressure)
//!   sets the output watermark that pauses normal frame dispatch.
//!   [`set_write_timeout`](ntex_io::IoConfig::set_write_timeout) limits how
//!   long each backpressure period may last, not how long an individual
//!   response may take.
//! - [`set_shutdown_timeout`](ntex_io::IoConfig::set_shutdown_timeout) bounds
//!   transport shutdown and, separately, the dispatcher's wait for readiness
//!   during stop delivery. It is not a general deadline for service calls or
//!   service shutdown; see [`Dispatcher`].
//!
//! Keep-alive, frame-read, and write timeouts are disabled by default.
//! [`Dispatcher::max_inflight`] separately limits normal frame admission;
//! it does not limit buffer sizes.
#![deny(clippy::pedantic)]
#![allow(clippy::cast_possible_truncation)]
use std::task::{Context, Poll, ready};
use std::time::Instant;
use std::{cell::Cell, fmt, future::Future, future::poll_fn, io, mem, pin::Pin, rc::Rc};

use ntex_codec::{Decoder, Encoder};
use ntex_io::{CloseReason, Decoded, IoBoxed, IoStatusUpdate, RecvError};
use ntex_service::pipeline::{Pipeline, PipelineCall};
use ntex_util::{spawn_detached, time::Seconds, time::Sleep, time::sleep};

mod timer;

use self::timer::{Timer, Timers};

type Response<U> = <U as Encoder>::Item;

/// Event delivered to the dispatcher service.
///
/// The codec's [`Decoder::Item`] is the request payload, while its
/// [`Encoder::Item`] is the response payload; these types need not match.
/// Every variant is passed through the same pipeline and may produce a
/// response, no response, or a service error.
///
/// Event calls can overlap. In particular, receiving [`Self::Stop`] does not
/// mean earlier item calls have finished. See [`Dispatcher`] for readiness,
/// response ordering, and shutdown behavior.
pub enum DispatchItem<U: Encoder + Decoder> {
    /// A frame decoded from the transport.
    ///
    /// Produced only when [`Decoder::decode`] returns `Some(item)`. The
    /// decoder determines frame boundaries; a transport read need not
    /// correspond to one service call.
    Item(<U as Decoder>::Item),
    /// A transport flow-control notification.
    ///
    /// These describe write-buffer pressure, not an incoming protocol frame.
    Control(Control),
    /// The dispatcher is stopping for the specified reason.
    ///
    /// The dispatcher attempts to deliver this through the pipeline before
    /// shutting down the service. Delivery waits for service readiness and
    /// may be handed to a background task if that wait exceeds the shutdown
    /// timeout. A readiness error can prevent the service from receiving it.
    Stop(Reason<U>),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
/// Write-side flow-control notification.
///
/// Delivered as [`DispatchItem::Control`] through the regular service pipeline.
/// Thresholds come from
/// [`IoConfig::set_write_backpressure`](ntex_io::IoConfig::set_write_backpressure).
/// These notifications do not themselves acknowledge delivery of a response
/// to the peer.
pub enum Control {
    /// Write backpressure has been enabled.
    ///
    /// Normal frame decoding pauses while outstanding output is above the
    /// transport's release threshold. Already-started service calls continue
    /// and can produce additional output.
    WBackPressureEnabled,
    /// Write backpressure has been disabled.
    ///
    /// Enough output has drained to resume normal frame processing. Some
    /// bytes may still be buffered; this is not a full-flush notification.
    WBackPressureDisabled,
}

/// Reason a dispatcher is stopping.
///
/// Carried by [`DispatchItem::Stop`], rather than returned as the dispatcher's
/// error type. The dispatcher also reports the corresponding
/// [`ntex_io::CloseReason`] to the transport. An already-recorded transport
/// close reason is not overwritten.
///
/// A stop caused by I/O, codec, or timeout failure can still yield `Ok(())`
/// from the dispatcher if no service error is recorded. Inspect the stop
/// event when the application needs to distinguish those outcomes.
pub enum Reason<U: Encoder + Decoder> {
    /// A service readiness check or event call failed.
    ///
    /// The service error is retained for the dispatcher's result; this
    /// notification does not contain or clone it.
    Service,
    /// The transport closed or failed.
    ///
    /// `None` indicates closure without an underlying I/O error, including
    /// clean peer EOF. `Some(error)` contains a transport error or an
    /// [`io::ErrorKind::UnexpectedEof`] synthesized when peer EOF leaves
    /// undecodable bytes in the read buffer.
    Io(Option<io::Error>),
    /// A service response could not be encoded.
    ///
    /// Contains the error returned by [`Encoder::encode`].
    Encoder(<U as Encoder>::Error),
    /// Incoming bytes could not be decoded.
    ///
    /// Contains the error returned by [`Decoder::decode`].
    Decoder(<U as Decoder>::Error),
    /// The connection's idle timeout expired.
    ///
    /// Usually caused by the configured keep-alive timeout. An external
    /// [`ntex_io::IoRef::notify_timeout`] notification can also produce this
    /// reason when no service call is in flight, even if keep-alive is disabled.
    KeepAlive,
    /// Frame-read progress was insufficient or its cumulative budget expired.
    ///
    /// Controlled by
    /// [`IoConfig::set_frame_read_rate`](ntex_io::IoConfig::set_frame_read_rate).
    /// Completing a frame ends its read-rate tracking; subsequent idle time
    /// is governed by keep-alive instead.
    ReadTimeout,
    /// Write backpressure stayed enabled for longer than the configured
    /// write timeout.
    ///
    /// Small writes do not extend the deadline unless they release
    /// backpressure. Once pressure is released, the timeout ends even if
    /// some output remains buffered. See
    /// [`IoConfig::set_write_timeout`](ntex_io::IoConfig::set_write_timeout).
    WriteTimeout,
}

/// Reports a truncated stream, the peer closed cleanly in the middle of a frame.
fn truncated(io: &IoBoxed) -> Option<io::Error> {
    if io.is_read_eof() && io.read_dst_size() != 0 {
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
/// Owns the transport, codec, and service pipeline supplied to [`Self::new`].
/// The pipeline accepts [`DispatchItem<U>`] and returns
/// `Option<<U as Encoder>::Item>`. Successful responses are encoded into the
/// transport's write buffer; `None` produces no response.
///
/// Poll or await this local, non-`Send` future on the runtime owning the
/// transport. Its output is `Result<(), Err>`; construction alone does not
/// dispatch service events.
///
/// # Readiness and concurrency
///
/// Normal frame dispatch waits for pipeline readiness and space below the
/// [`Self::max_inflight`] threshold. Service calls may overlap, and responses
/// are encoded as calls complete rather than being reordered into request
/// order. Use `max_inflight(1)` to serialize normal frame handling.
///
/// Write backpressure pauses frame decoding and produces
/// [`Control::WBackPressureEnabled`]. When enough output drains, the dispatcher
/// produces [`Control::WBackPressureDisabled`] and resumes normal processing.
/// Pending calls continue during backpressure and may add more output.
/// Control and stop events use the same readiness-aware pipeline as frames,
/// but are not constrained by the normal frame-admission limit.
///
/// Pauses for service readiness or in-flight capacity suspend keep-alive and
/// frame-read timers and reset the frame-read budget. A write-backpressure
/// timeout continues running during these pauses. A paused transport filter
/// suspends read-side and write timeouts instead, so filter delays are not
/// charged to the peer.
///
/// # Stopping and shutdown
///
/// On a stop condition, the dispatcher attempts a [`DispatchItem::Stop`] call.
/// That call can overlap earlier requests. Normally, after the stop call
/// completes, the dispatcher drains all remaining service calls, waits for
/// service shutdown, and then finishes transport shutdown. This drain also
/// applies to transport failures; it has no general service-call deadline.
///
/// If pending stop delivery is blocked by readiness beyond the transport's
/// [`shutdown_timeout`](ntex_io::IoConfig::shutdown_timeout), stop delivery,
/// draining, and service shutdown continue in a background task while the
/// dispatcher finishes transport shutdown. At this handoff, the response
/// future held by the dispatcher is cancelled; already-detached calls
/// continue to drain. The dispatcher may therefore complete before service
/// shutdown, and errors reported after handoff are not returned by it.
///
/// Only service errors are returned as `Err`; other stop reasons are reported
/// as events. A service error recorded before stop delivery takes precedence
/// over a later stop-handler error. If none was recorded, a stop-handler error
/// is returned when that call completes before handoff.
///
/// Dropping the dispatcher cancels the call futures it owns, but does not
/// perform the normal stop-and-shutdown sequence or cancel already-detached
/// calls. Drive the future to completion when orderly shutdown is required.
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

type Service<U, Err> = Pipeline<DispatchItem<U>, Option<Response<U>>, Err>;

struct Call<U: Encoder + Decoder, Err> {
    // Drop the service future before releasing its count and waking shutdown.
    fut: PipelineCall<DispatchItem<U>, Option<Response<U>>, Err>,
    guard: CallGuard<U, Err>,
}

struct CallGuard<U: Encoder + Decoder, Err> {
    shared: Rc<DispatcherShared<U, Err>>,
    wake: bool,
}

impl<U: Encoder + Decoder, Err> Drop for CallGuard<U, Err> {
    fn drop(&mut self) {
        self.shared.inflight.set(self.shared.inflight.get() - 1);
        if self.wake {
            self.shared.io.notify_dispatcher();
        }
    }
}

impl<U: Encoder + Decoder, Err> fmt::Debug for Call<U, Err> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.fut.fmt(f)
    }
}

impl<U: Encoder + Decoder + 'static, Err: 'static> Future for Call<U, Err> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        let result = ready!(Pin::new(&mut this.fut).poll(cx));
        this.guard.shared.handle_result(result);
        this.guard.wake = false;
        Poll::Ready(())
    }
}

struct DispatcherInner<U, Err>
where
    U: Encoder + Decoder + 'static,
{
    st: DispatcherState<U, Err>,
    error: Option<Err>,
    shared: Rc<DispatcherShared<U, Err>>,
    response: Option<Call<U, Err>>,
    timers: Timers,
    max_inflight: u32,
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
    Stop(StopCall<U, Err>),
    Shutdown,
    ShutdownIo,
}

#[derive(Debug)]
struct StopCall<U: Encoder + Decoder, Err> {
    fut: Call<U, Err>,
    // started while the stop call waits for service readiness
    timeout: Option<Sleep>,
}

#[derive(Debug)]
enum DispatcherError<S, U> {
    Encoder(U),
    Service(S),
}

enum PollService {
    /// Write backpressure is enabled while the service is not ready.
    Backpressure,
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
    /// Accepts any transport convertible to [`IoBoxed`], including an
    /// [`ntex_io::Io`]. The codec must decode incoming frames and encode service
    /// responses, which may have different item types.
    ///
    /// The pipeline must accept [`DispatchItem<U>`] and return
    /// `Result<Option<<U as Encoder>::Item>, Err>`. It already owns its service
    /// state and binding; this constructor does not bind the service again or
    /// invoke it.
    ///
    /// Timeout and buffer policies come from the transport's
    /// [`ntex_io::IoConfig`]. Construction clears any previous dispatcher timer
    /// and starts first-frame read-rate monitoring immediately when configured,
    /// before this future is first polled. Construct it on the runtime owning
    /// the transport, then poll or await it to drive dispatch.
    ///
    /// The default in-flight threshold is [`u32::MAX`]. Use
    /// [`Self::max_inflight`] to restrict normal frame admission.
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
                max_inflight: u32::MAX,
            },
        }
    }

    #[must_use]
    /// Limits normal frame admission by the number of in-flight service calls.
    ///
    /// While at least `max` calls are pending, the dispatcher pauses normal
    /// frame dispatch. It resumes when the count falls below `max` and the
    /// service is ready. Set this to `1` to serialize normal frame handling.
    ///
    /// Pending control and stop calls contribute to the count, but their
    /// delivery can exceed this threshold so that shutdown and flow-control
    /// processing can proceed. This is not a strict cap on all event calls or
    /// on the transport's input and output buffers.
    ///
    /// The default is [`u32::MAX`], imposing no practical dispatcher-level
    /// concurrency limit.
    ///
    /// # Panics
    ///
    /// Panics if `max` is zero.
    pub fn max_inflight(mut self, max: u32) -> Self {
        assert!(max > 0, "max_inflight must be greater than zero");
        self.inner.max_inflight = max;
        self
    }
}

impl<U, Err> DispatcherShared<U, Err>
where
    U: Encoder + Decoder + 'static,
    Err: 'static,
{
    fn call(self: &Rc<Self>, item: DispatchItem<U>) -> Call<U, Err> {
        self.inflight.set(self.inflight.get() + 1);
        let guard = CallGuard {
            shared: self.clone(),
            wake: true,
        };
        Call {
            fut: self.service.call_static(item),
            guard,
        }
    }

    fn handle_result(&self, item: Result<Option<Response<U>>, Err>) {
        match item {
            Ok(Some(val)) => {
                if let Err(err) = self.io.encode(val, &self.codec) {
                    self.error.set(Some(DispatcherError::Encoder(err)));
                }
            }
            Err(err) => self.error.set(Some(DispatcherError::Service(err))),
            Ok(None) => (),
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
            && Pin::new(fut).poll(cx).is_ready()
        {
            inner.response = None;
        }

        loop {
            match inner.st {
                DispatcherState::Processing => {
                    let item = match ready!(inner.poll_service(cx)) {
                        PollService::Ready => {
                            // decode incoming bytes if buffer is ready
                            match inner.shared.io.poll_recv_decode(&inner.shared.codec, cx) {
                                Ok(decoded) => {
                                    inner.update_timer(&decoded);
                                    if let Some(el) = decoded.item {
                                        DispatchItem::Item(el)
                                    } else {
                                        return Poll::Pending;
                                    }
                                }
                                Err(RecvError::Timeout) if inner.timers.active != Timer::Write => {
                                    // input received together with a read timer wins over
                                    // its expiry, the buffered input is decoded first
                                    match inner.shared.io.decode_item(&inner.shared.codec) {
                                        Ok(decoded) => {
                                            let timer = inner.timers.active;
                                            inner.update_timer(&decoded);
                                            if let Some(el) = decoded.item {
                                                DispatchItem::Item(el)
                                            } else {
                                                // a timer armed for the received input has
                                                // not expired
                                                if inner.timers.active == timer
                                                    && let Err(ctl) = inner.handle_timeout()
                                                {
                                                    inner.st = inner.stop(ctl);
                                                }
                                                continue;
                                            }
                                        }
                                        Err(err) => {
                                            log::trace!(
                                                "{}: Decoder error, stopping dispatcher: {:?}",
                                                inner.shared.io.tag(),
                                                err
                                            );
                                            inner.st = inner.stop(Reason::Decoder(err));
                                            continue;
                                        }
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
                                    DispatchItem::Control(Control::WBackPressureEnabled)
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
                        PollService::Backpressure => {
                            DispatchItem::Control(Control::WBackPressureEnabled)
                        }
                        PollService::Continue => continue,
                    };

                    inner.call_service(cx, item);
                }
                // handle write back-pressure
                DispatcherState::Backpressure => {
                    match ready!(inner.poll_service(cx)) {
                        PollService::Ready | PollService::Backpressure => {}
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
                    // suspends or resumes the write timeout on a filter pause
                    inner.start_write_timer();

                    let item = if let Err(err) = ready!(inner.shared.io.poll_flush(cx, false)) {
                        inner.st = inner.stop(Reason::Io(Some(err)));
                        continue;
                    } else {
                        // Stops the write timeout when write backpressure is disabled.
                        inner.stop_timer();
                        inner.st = DispatcherState::Processing;
                        DispatchItem::Control(Control::WBackPressureDisabled)
                    };
                    inner.call_service(cx, item);
                }
                // deliver stop to service
                DispatcherState::Stop(ref mut stop) => {
                    // service may relay on poll_ready for response results
                    let ready = inner.shared.service.poll_ready(cx);

                    if Pin::new(&mut stop.fut).poll(cx).is_ready() {
                        // the dispatcher returns a service error from the stop call,
                        // unless it is stopping because of an earlier one
                        if let Some(DispatcherError::Service(err)) = inner.shared.error.take()
                            && inner.error.is_none()
                        {
                            inner.error = Some(err);
                        }
                        inner.shared.io.stop_timer();
                        inner.st = DispatcherState::Shutdown;
                        continue;
                    }
                    if ready.is_ready() {
                        return Poll::Pending;
                    }

                    // the stop item waits for service readiness, it is delivered
                    // in the background and io is closed without waiting for it
                    let timeout = stop
                        .timeout
                        .get_or_insert_with(|| sleep(inner.shared.io.cfg().shutdown_timeout()));
                    ready!(timeout.poll_elapsed(cx));
                    log::trace!(
                        "{}: Service is not ready for stop, shutdown io",
                        inner.shared.io.tag()
                    );

                    let DispatcherState::Stop(stop) =
                        mem::replace(&mut inner.st, DispatcherState::ShutdownIo)
                    else {
                        unreachable!()
                    };
                    inner.shared.io.stop_timer();
                    inner.response = None;

                    // the task owns the readiness check from now on
                    let shared = inner.shared.clone();
                    let mut fut = Some(stop.fut);
                    spawn_detached(poll_fn(move |cx| {
                        let _ = shared.service.poll_ready(cx);
                        if let Some(f) = fut.as_mut() {
                            ready!(Pin::new(f).poll(cx));
                            fut = None;
                        }
                        if shared.inflight.get() != 0 {
                            shared.io.register_dispatch(cx);
                            return Poll::Pending;
                        }
                        shared.service.poll_shutdown(cx)
                    }));
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
        self.shared.io.set_close_reason(match reason {
            Reason::Service => CloseReason::Service,
            Reason::Io(None) => CloseReason::PeerGone,
            Reason::Io(Some(_)) => CloseReason::Io,
            Reason::Encoder(_) => CloseReason::Encoder,
            Reason::Decoder(_) => CloseReason::Decoder,
            Reason::KeepAlive => CloseReason::KeepAlive,
            Reason::ReadTimeout => CloseReason::ReadTimeout,
            Reason::WriteTimeout => CloseReason::WriteTimeout,
        });
        DispatcherState::Stop(StopCall {
            fut: self.shared.call(DispatchItem::Stop(reason)),
            timeout: None,
        })
    }

    fn call_service(&mut self, cx: &mut Context<'_>, item: DispatchItem<U>) {
        let mut fut = self.shared.call(item);

        // the first poll consumes the pipeline readiness checked by `poll_service()`
        if Pin::new(&mut fut).poll(cx).is_ready() {
            drop(fut);
        } else if self.response.is_none() {
            self.response = Some(fut);
        } else {
            let shared = self.shared.clone();
            spawn_detached(async move {
                fut.await;
                shared.io.notify_dispatcher();
            });
        }
    }

    fn check_error(&mut self) -> PollService {
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

    fn poll_service(&mut self, cx: &mut Context<'_>) -> Poll<PollService> {
        // wait until an in-flight call completes and the service becomes ready
        let ready = if self.shared.inflight.get() >= self.max_inflight {
            Poll::Pending
        } else {
            self.shared.service.poll_ready(cx)
        };
        match ready {
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
                    // frames that were already received are dispatched once
                    // the service is ready, as in the processing state
                    IoStatusUpdate::PeerGone(_) if self.shared.io.read_dst_size() != 0 => {
                        log::trace!(
                            "{}: Peer is gone during pause, wait for service",
                            self.shared.io.tag()
                        );
                        Poll::Pending
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
                        Poll::Ready(PollService::Backpressure)
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
    /// stopped when no write timeout is configured. The timeout does not run
    /// while the filter chain is paused, it holds output back and the peer is
    /// not charged for it. The timeout starts over once the chain resumes.
    fn start_write_timer(&mut self) {
        let timeout = self.shared.io.cfg().write_timeout();
        if timeout.is_zero() || self.shared.io.is_filter_paused() {
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

        // the filter chain pauses reading, the peer is not charged for it
        if !item && self.shared.io.is_filter_paused() {
            if self.timers.active != Timer::Write {
                self.stop_timer();
            }
            self.timers.reset_read(self.shared.io.cfg());
            return;
        }

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
    ///
    /// The keep-alive timer is suspended instead of stopped, re-arming it is
    /// cheaper than registering it again.
    fn set_timer(&mut self, timer: Timer) {
        if self.timers.active == timer {
            return;
        }
        let io = &self.shared.io;
        self.timers.active = match (self.timers.active, timer) {
            (Timer::KeepAlive | Timer::Suspended, Timer::Stopped) => Timer::Suspended,
            (_, Timer::KeepAlive) => {
                log::trace!(
                    "{}: Start keep-alive timer {:?}",
                    io.tag(),
                    io.cfg().keepalive_timeout()
                );
                let hnd = io.start_timer(io.cfg().keepalive_timeout());
                self.timers.keepalive = Some(hnd.instant());
                Timer::KeepAlive
            }
            (_, Timer::FrameRead) if let Some(params) = io.cfg().frame_read_rate() => {
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
            // the filter chain paused before the dispatcher noticed
            Timer::Write if self.shared.io.is_filter_paused() => {
                self.stop_timer();
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
            // the suspended keep-alive timer has expired, a timeout before its
            // expiry is external
            Timer::Suspended
                if self
                    .timers
                    .keepalive
                    .is_none_or(|expiry| Instant::now() >= expiry) =>
            {
                self.timers.active = Timer::Stopped;
                Ok(())
            }
            // external timeout, applies to idle connection
            Timer::Stopped | Timer::Suspended if self.shared.inflight.get() == 0 => {
                log::trace!(
                    "{}: Idle timeout, stopping dispatcher",
                    self.shared.io.tag()
                );
                Err(Reason::KeepAlive)
            }
            Timer::Stopped | Timer::Suspended => Ok(()),
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
#[path = "../tests/unit/dispatcher.rs"]
mod tests;
