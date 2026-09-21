//! Service dispatcher for framed I/O transports.
//!
//! [`Dispatcher`] reads frames from an `ntex-io` transport using an
//! `ntex-codec` decoder and forwards them to an `ntex-service` pipeline as
//! [`DispatchItem`] values. The service may return an encoded response, report
//! a service error, or return `None` when no response is required.
//!
//! The dispatcher also reports write backpressure through [`Control`] messages
//! and delivers disconnect, codec, keep-alive, and frame-read failures through
//! [`Reason`] before shutting down the service.
#![deny(clippy::pedantic)]
#![allow(clippy::cast_possible_truncation)]
use std::task::{Context, Poll, ready};
use std::{cell::Cell, fmt, future::Future, io, pin::Pin, rc::Rc};

use ntex_codec::{Decoder, Encoder};
use ntex_io::{Decoded, IoBoxed, IoStatusUpdate, RecvError};
use ntex_service::pipeline::{Pipeline, PipelineCall};
use ntex_util::{future::Either, spawn, time::Seconds};

type Response<U> = <U as Encoder>::Item;

fn next_read_timeout(timeout: Seconds, max_timeout: Seconds) -> (Seconds, Seconds) {
    if max_timeout.is_zero() {
        (timeout, Seconds::ZERO)
    } else {
        let timeout = Seconds(timeout.0.min(max_timeout.0));
        (timeout, Seconds(max_timeout.0.saturating_sub(timeout.0)))
    }
}

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
    /// The transport disconnected.
    ///
    /// The value contains the underlying I/O error when one was available.
    Io(Option<io::Error>),
    /// A service response could not be encoded.
    Encoder(<U as Encoder>::Error),
    /// Incoming bytes could not be decoded.
    Decoder(<U as Decoder>::Error),
    /// The connection exceeded its keep-alive timeout.
    KeepAliveTimeout,
    /// The frame did not maintain the configured read rate or exceeded its
    /// cumulative read timeout.
    ReadTimeout,
}

pin_project_lite::pin_project! {
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
        U: Encoder,
        U: Decoder,
        U: 'static,
        Err: 'static,
    {
        inner: DispatcherInner<U, Err>,
    }
}

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
    struct Flags: u8  {
        const READY_ERR     = 0b000_0001;
        const IO_ERR        = 0b000_0010;
        const KA_ENABLED    = 0b000_0100;
        const KA_TIMEOUT    = 0b000_1000;
        const READ_TIMEOUT  = 0b001_0000;
        const IDLE          = 0b010_0000;
    }
}

struct DispatcherInner<U, Err>
where
    U: Encoder + Decoder + 'static,
{
    st: DispatcherState,
    error: Option<Err>,
    shared: Rc<DispatcherShared<U, Err>>,
    response: Option<PipelineCall<DispatchItem<U>, Option<Response<U>>, Err>>,
    read_state: ReadState,
}

pub(crate) struct DispatcherShared<U, Err>
where
    U: Encoder + Decoder,
{
    io: IoBoxed,
    codec: U,
    service: Pipeline<DispatchItem<U>, Option<Response<U>>, Err>,
    flags: Cell<Flags>,
    error: Cell<Option<DispatcherError<Err, <U as Encoder>::Error>>>,
    inflight: Cell<u32>,
}

#[derive(Copy, Clone, Debug)]
enum DispatcherState {
    Processing,
    Backpressure,
    Stop,
    Shutdown,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum ReadState {
    Idle,
    FirstFrame(ReadProgress),
    ReadingFrame(ReadProgress),
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct ReadProgress {
    remains: u32,
    consumed: u32,
    max_timeout: Seconds,
}

impl ReadState {
    #[cfg(test)]
    fn progress(&self) -> Option<&ReadProgress> {
        match self {
            Self::FirstFrame(progress) | Self::ReadingFrame(progress) => Some(progress),
            Self::Idle => None,
        }
    }

    fn progress_mut(&mut self) -> Option<&mut ReadProgress> {
        match self {
            Self::FirstFrame(progress) | Self::ReadingFrame(progress) => Some(progress),
            Self::Idle => None,
        }
    }
}

#[derive(Debug)]
enum DispatcherError<S, U> {
    Encoder(U),
    Service(S),
}

enum PollService<U: Encoder + Decoder> {
    Item(DispatchItem<U>),
    ItemWait(DispatchItem<U>),
    Continue,
    Ready,
}

impl<S, U> From<Either<S, U>> for DispatcherError<S, U> {
    fn from(err: Either<S, U>) -> Self {
        match err {
            Either::Left(err) => DispatcherError::Service(err),
            Either::Right(err) => DispatcherError::Encoder(err),
        }
    }
}

impl<U, Err> Dispatcher<U, Err>
where
    U: Decoder + Encoder + 'static,
    Err: 'static,
{
    /// Creates a dispatcher for an I/O transport, codec, and service pipeline.
    ///
    /// Keep-alive and frame-read timeout behavior is taken from the transport's
    /// `ntex_io::IoConfig`. When frame read-rate enforcement is configured, its
    /// first measurement interval starts immediately for the new connection.
    pub fn new<Io>(
        io: Io,
        codec: U,
        service: Pipeline<DispatchItem<U>, Option<Response<U>>, Err>,
    ) -> Dispatcher<U, Err>
    where
        IoBoxed: From<Io>,
    {
        let io = IoBoxed::from(io);
        let mut flags = if io.cfg().keepalive_timeout().is_zero() {
            Flags::empty()
        } else {
            Flags::KA_ENABLED
        };

        let read_state = if let Some(cfg) = io.cfg().frame_read_rate() {
            let (timeout, max_timeout) = next_read_timeout(cfg.timeout, cfg.max_timeout);
            flags.insert(Flags::READ_TIMEOUT);
            io.start_timer(timeout);
            ReadState::FirstFrame(ReadProgress {
                remains: 0,
                consumed: 0,
                max_timeout,
            })
        } else {
            ReadState::Idle
        };

        let shared = Rc::new(DispatcherShared {
            io,
            codec,
            service,
            flags: Cell::new(flags),
            error: Cell::new(None),
            inflight: Cell::new(0),
        });

        Dispatcher {
            inner: DispatcherInner {
                shared,
                response: None,
                error: None,
                read_state,
                st: DispatcherState::Processing,
            },
        }
    }
}

impl<U, Err> DispatcherShared<U, Err>
where
    U: Encoder + Decoder,
{
    fn handle_result(&self, item: Result<Option<Response<U>>, Err>, io: &IoBoxed, wake: bool) {
        match item {
            Ok(Some(val)) => {
                if let Err(err) = io.encode(val, &self.codec) {
                    self.error.set(Some(DispatcherError::Encoder(err)));
                }
            }
            Err(err) => self.error.set(Some(DispatcherError::Service(err))),
            Ok(None) => (),
        }
        let inflight = self.inflight.get() - 1;
        self.inflight.set(inflight);
        if inflight == 0 {
            self.insert_flags(Flags::IDLE);
        }
        if wake {
            io.notify_dispatcher();
        }
    }

    fn contains(&self, f: Flags) -> bool {
        self.flags.get().intersects(f)
    }

    fn insert_flags(&self, f: Flags) {
        let mut flags = self.flags.get();
        flags.insert(f);
        self.flags.set(flags);
    }

    fn remove_flags(&self, f: Flags) -> bool {
        let mut flags = self.flags.get();
        if flags.intersects(f) {
            flags.remove(f);
            self.flags.set(flags);
            true
        } else {
            false
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
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.as_mut().project();
        let inner = this.inner;

        // handle service response future
        if let Some(fut) = inner.response.as_mut()
            && let Poll::Ready(item) = Pin::new(fut).poll(cx)
        {
            inner.shared.handle_result(item, &inner.shared.io, false);
            inner.response = None;
        }

        loop {
            match inner.st {
                DispatcherState::Processing => {
                    let (item, nowait) = match ready!(inner.poll_service(cx)) {
                        PollService::Ready => {
                            // decode incoming bytes if buffer is ready
                            inner.update_read_progress();
                            match inner.shared.io.poll_recv_decode(&inner.shared.codec, cx) {
                                Ok(decoded) => {
                                    inner.update_timer(&decoded);
                                    if let Some(el) = decoded.item {
                                        (DispatchItem::Item(el), true)
                                    } else {
                                        return Poll::Pending;
                                    }
                                }
                                Err(RecvError::KeepAlive) => {
                                    if let Err(ctl) = inner.handle_timeout() {
                                        inner.st = DispatcherState::Stop;
                                        (DispatchItem::Stop(ctl), true)
                                    } else {
                                        continue;
                                    }
                                }
                                Err(RecvError::WriteBackpressure) => {
                                    // instruct write task to notify dispatcher when data is flushed
                                    inner.st = DispatcherState::Backpressure;
                                    (DispatchItem::Control(Control::WBackPressureEnabled), true)
                                }
                                Err(RecvError::Decoder(err)) => {
                                    log::trace!(
                                        "{}: Decoder error, stopping dispatcher: {:?}",
                                        inner.shared.io.tag(),
                                        err
                                    );
                                    inner.st = DispatcherState::Stop;
                                    (DispatchItem::Stop(Reason::Decoder(err)), true)
                                }
                                Err(RecvError::PeerGone(err)) => {
                                    log::trace!(
                                        "{}: Peer is gone, stopping dispatcher: {:?}",
                                        inner.shared.io.tag(),
                                        err
                                    );
                                    if err.is_some() || inner.shared.io.is_terminating() {
                                        inner.shared.insert_flags(Flags::IO_ERR);
                                    }
                                    inner.st = DispatcherState::Stop;
                                    (DispatchItem::Stop(Reason::Io(err)), true)
                                }
                            }
                        }
                        PollService::Item(item) => (item, true),
                        PollService::ItemWait(item) => (item, false),
                        PollService::Continue => continue,
                    };

                    inner.call_service(cx, item, nowait);
                }
                // handle write back-pressure
                DispatcherState::Backpressure => {
                    match ready!(inner.poll_service(cx)) {
                        PollService::Ready
                        | PollService::ItemWait(DispatchItem::Control(
                            Control::WBackPressureEnabled,
                        )) => (),
                        PollService::Item(item) => inner.call_service(cx, item, true),
                        PollService::ItemWait(item) => inner.call_service(cx, item, false),
                        PollService::Continue => continue,
                    }

                    let item = if let Err(err) = ready!(inner.shared.io.poll_flush(cx, false)) {
                        inner.shared.insert_flags(Flags::IO_ERR);
                        inner.st = DispatcherState::Stop;
                        DispatchItem::Stop(Reason::Io(Some(err)))
                    } else {
                        inner.st = DispatcherState::Processing;
                        DispatchItem::Control(Control::WBackPressureDisabled)
                    };
                    inner.call_service(cx, item, false);
                }
                // drain service responses and shutdown io
                DispatcherState::Stop => {
                    inner.shared.io.stop_timer();

                    if inner.shared.contains(Flags::IO_ERR) {
                        inner.response = None;
                        return if inner.shared.io.poll_shutdown(cx).is_ready() {
                            Poll::Ready(if let Some(err) = inner.error.take() {
                                Err(err)
                            } else {
                                Ok(())
                            })
                        } else {
                            Poll::Pending
                        };
                    }

                    // service may relay on poll_ready for response results
                    if !inner.shared.contains(Flags::READY_ERR)
                        && let Poll::Ready(res) = inner.shared.service.poll_ready(cx)
                        && res.is_err()
                    {
                        inner.shared.insert_flags(Flags::READY_ERR);
                    }

                    if inner.shared.inflight.get() == 0 {
                        match inner.shared.io.poll_shutdown(cx) {
                            Poll::Ready(Ok(())) => {
                                inner.st = DispatcherState::Shutdown;
                                continue;
                            }
                            Poll::Ready(Err(_)) => {
                                inner.shared.insert_flags(Flags::IO_ERR);
                                continue;
                            }
                            Poll::Pending => (),
                        }
                    } else if inner.shared.io.is_terminating() {
                        inner.shared.insert_flags(Flags::IO_ERR);
                        continue;
                    } else if inner.shared.io.is_closed() {
                        inner.shared.io.poll_dispatch(cx);
                    } else if !inner.shared.contains(Flags::IO_ERR) {
                        match ready!(inner.shared.io.poll_status_update(cx)) {
                            IoStatusUpdate::PeerGone(_) => {
                                if inner.shared.io.is_terminating() {
                                    inner.shared.insert_flags(Flags::IO_ERR);
                                    continue;
                                }
                                inner.shared.io.poll_dispatch(cx);
                            }
                            IoStatusUpdate::KeepAlive => continue,
                            IoStatusUpdate::WriteBackpressure => {
                                if ready!(inner.shared.io.poll_flush(cx, true)).is_err() {
                                    inner.shared.insert_flags(Flags::IO_ERR);
                                }
                                continue;
                            }
                        }
                    } else {
                        inner.shared.io.poll_dispatch(cx);
                    }
                    return Poll::Pending;
                }
                // shutdown service
                DispatcherState::Shutdown => {
                    if inner.shared.contains(Flags::IO_ERR) {
                        return Poll::Ready(if let Some(err) = inner.error.take() {
                            Err(err)
                        } else {
                            Ok(())
                        });
                    }

                    return if inner.shared.service.poll_shutdown(cx).is_ready() {
                        log::trace!(
                            "{}: Service shutdown is completed, stop",
                            inner.shared.io.tag()
                        );

                        Poll::Ready(if let Some(err) = inner.error.take() {
                            Err(err)
                        } else {
                            Ok(())
                        })
                    } else {
                        Poll::Pending
                    };
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
    fn update_read_progress(&mut self) {
        if self.shared.contains(Flags::READ_TIMEOUT) {
            let buffered = self.shared.io.with_read_buf(|buf| buf.len()) as u32;
            let progress = self
                .read_state
                .progress_mut()
                .expect("read timeout requires active frame progress");
            progress.consumed = progress
                .consumed
                .saturating_add(buffered.saturating_sub(progress.remains));
            progress.remains = buffered;
        }
    }

    fn start_read_timer(&mut self, consumed: u32, remains: u32) {
        if let Some(params) = self.shared.io.cfg().frame_read_rate() {
            self.shared.remove_flags(Flags::KA_TIMEOUT | Flags::IDLE);
            self.shared.insert_flags(Flags::READ_TIMEOUT);

            let (timeout, max_timeout) = next_read_timeout(params.timeout, params.max_timeout);
            let progress = ReadProgress {
                remains,
                consumed,
                max_timeout,
            };
            self.read_state = if consumed != 0
                || remains != 0
                || matches!(self.read_state, ReadState::ReadingFrame(_))
            {
                ReadState::ReadingFrame(progress)
            } else {
                ReadState::FirstFrame(progress)
            };
            self.shared.io.start_timer(timeout);
        }
    }

    fn call_service(&mut self, cx: &mut Context<'_>, item: DispatchItem<U>, nowait: bool) {
        let mut fut = if nowait {
            self.shared.service.call_nowait(item)
        } else {
            self.shared.service.call_static(item)
        };
        let inflight = self.shared.inflight.get() + 1;
        self.shared.inflight.set(inflight);
        if inflight == 1 {
            self.shared.remove_flags(Flags::IDLE);
        }

        // optimize first call
        if self.response.is_none() {
            if let Poll::Ready(result) = Pin::new(&mut fut).poll(cx) {
                self.shared.handle_result(result, &self.shared.io, false);
            } else {
                self.response = Some(fut);
            }
        } else {
            let shared = self.shared.clone();
            spawn(async move {
                let result = fut.await;
                shared.handle_result(result, &shared.io, true);
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
            self.st = DispatcherState::Stop;

            match err {
                DispatcherError::Encoder(err) => {
                    PollService::Item(DispatchItem::Stop(Reason::Encoder(err)))
                }
                DispatcherError::Service(err) => {
                    self.error = Some(err);
                    PollService::Continue
                }
            }
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

                let read_timeout = self.shared.contains(Flags::READ_TIMEOUT);
                if read_timeout {
                    self.update_read_progress();
                }

                // remove all timers
                let timeout_pending = self.shared.io.stop_timer_status();
                let timeout_reason = if read_timeout && timeout_pending {
                    self.handle_timeout().err()
                } else {
                    None
                };
                self.shared
                    .remove_flags(Flags::KA_TIMEOUT | Flags::READ_TIMEOUT | Flags::IDLE);
                self.shared.io.stop_timer();

                if let Some(reason) = timeout_reason {
                    log::trace!(
                        "{}: Frame read timeout during service pause",
                        self.shared.io.tag()
                    );
                    self.st = DispatcherState::Stop;
                    return Poll::Ready(PollService::ItemWait(DispatchItem::Stop(reason)));
                }

                match ready!(self.shared.io.poll_read_pause(cx)) {
                    IoStatusUpdate::KeepAlive => {
                        if self.shared.contains(Flags::KA_ENABLED) {
                            log::trace!(
                                "{}: Keep-alive error, stopping dispatcher during pause",
                                self.shared.io.tag()
                            );
                            self.st = DispatcherState::Stop;
                            Poll::Ready(PollService::ItemWait(DispatchItem::Stop(
                                Reason::KeepAliveTimeout,
                            )))
                        } else {
                            // ignore spurious DSP_TIMEOUT when keep-alive is disabled
                            Poll::Ready(PollService::Continue)
                        }
                    }
                    IoStatusUpdate::PeerGone(err) => {
                        log::trace!(
                            "{}: Peer is gone during pause, stopping dispatcher: {:?}",
                            self.shared.io.tag(),
                            err
                        );
                        if err.is_some() || self.shared.io.is_terminating() {
                            self.shared.insert_flags(Flags::IO_ERR);
                        }
                        self.st = DispatcherState::Stop;
                        Poll::Ready(PollService::ItemWait(DispatchItem::Stop(Reason::Io(err))))
                    }
                    IoStatusUpdate::WriteBackpressure => {
                        self.st = DispatcherState::Backpressure;
                        Poll::Ready(PollService::ItemWait(DispatchItem::Control(
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
                self.st = DispatcherState::Stop;
                self.error = Some(err);
                self.shared.insert_flags(Flags::READY_ERR);
                Poll::Ready(PollService::Continue)
            }
        }
    }

    fn update_timer(&mut self, decoded: &Decoded<<U as Decoder>::Item>) {
        // got parsed frame
        if decoded.item.is_some() {
            self.read_state = ReadState::Idle;
            self.shared
                .remove_flags(Flags::KA_TIMEOUT | Flags::READ_TIMEOUT | Flags::IDLE);
            self.shared.io.stop_timer();
        } else if self.shared.contains(Flags::READ_TIMEOUT) {
            // received new data but not enough for parsing complete frame
            self.read_state
                .progress_mut()
                .expect("read timeout requires active frame progress")
                .remains = decoded.remains as u32;
            if (decoded.consumed != 0 || decoded.remains != 0)
                && let ReadState::FirstFrame(progress) = self.read_state
            {
                self.read_state = ReadState::ReadingFrame(progress);
            }
        } else if matches!(
            self.read_state,
            ReadState::FirstFrame(_) | ReadState::ReadingFrame(_)
        ) {
            self.start_read_timer(
                (decoded.consumed as u32).saturating_add(decoded.remains as u32),
                decoded.remains as u32,
            );
        } else if decoded.remains == 0 && decoded.consumed == 0 {
            // no new data, start keep-alive timer
            if self.shared.contains(Flags::KA_ENABLED) && !self.shared.contains(Flags::KA_TIMEOUT) {
                log::trace!(
                    "{}: Start keep-alive timer {:?}",
                    self.shared.io.tag(),
                    self.shared.io.cfg().keepalive_timeout()
                );
                self.shared.insert_flags(Flags::KA_TIMEOUT);
                self.shared
                    .io
                    .start_timer(self.shared.io.cfg().keepalive_timeout());
            }
        } else {
            // we got new data but not enough to parse single frame
            self.start_read_timer(
                (decoded.consumed as u32).saturating_add(decoded.remains as u32),
                decoded.remains as u32,
            );
        }
    }

    fn handle_timeout(&mut self) -> Result<(), Reason<U>> {
        // check read timer
        if self.shared.contains(Flags::READ_TIMEOUT) {
            if let Some(params) = self.shared.io.cfg().frame_read_rate() {
                let progress = self
                    .read_state
                    .progress_mut()
                    .expect("read timeout requires active frame progress");
                let total = progress.consumed;
                progress.consumed = 0;

                // read rate, start timer for next period
                if total > params.rate {
                    let timeout = if params.max_timeout.is_zero() {
                        Some(params.timeout)
                    } else if progress.max_timeout.is_zero() {
                        None
                    } else {
                        let (timeout, remaining) =
                            next_read_timeout(params.timeout, progress.max_timeout);
                        progress.max_timeout = remaining;
                        Some(timeout)
                    };

                    if let Some(timeout) = timeout {
                        log::trace!(
                            "{}: Frame read rate {:?}, extend timer",
                            self.shared.io.tag(),
                            total
                        );
                        self.shared.io.start_timer(timeout);
                        return Ok(());
                    }
                    log::trace!(
                        "{}: Max payload timeout has been reached",
                        self.shared.io.tag()
                    );
                }
                Err(Reason::ReadTimeout)
            } else {
                Ok(())
            }
        } else if self.shared.contains(Flags::KA_TIMEOUT | Flags::IDLE) {
            log::trace!(
                "{}: Keep-alive error, stopping dispatcher",
                self.shared.io.tag()
            );
            Err(Reason::KeepAliveTimeout)
        } else {
            Ok(())
        }
    }
}

impl<U> fmt::Debug for DispatchItem<U>
where
    U: Encoder + Decoder,
    <U as Decoder>::Item: fmt::Debug,
{
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            DispatchItem::Item(ref item) => {
                write!(fmt, "DispatchItem::Item({item:?})")
            }
            DispatchItem::Control(ref e) => {
                write!(fmt, "DispatchItem::Control({e:?})")
            }
            DispatchItem::Stop(ref e) => {
                write!(fmt, "DispatchItem::Stop({e:?})")
            }
        }
    }
}

impl<U> fmt::Debug for Reason<U>
where
    U: Encoder + Decoder,
    <U as Encoder>::Error: fmt::Debug,
    <U as Decoder>::Error: fmt::Debug,
{
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Reason::Io(ref err) => {
                write!(fmt, "Reason::Io({err:?})")
            }
            Reason::Encoder(ref err) => {
                write!(fmt, "Reason::Encoder({err:?})")
            }
            Reason::Decoder(ref err) => {
                write!(fmt, "Reason::Decoder({err:?})")
            }
            Reason::KeepAliveTimeout => {
                write!(fmt, "Reason::KeepAliveTimeout")
            }
            Reason::ReadTimeout => {
                write!(fmt, "Reason::ReadTimeout")
            }
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

        fn encodev(&self, item: Bytes, dst: &mut BytePages) -> Result<(), Self::Error> {
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

    #[derive(Copy, Clone)]
    struct ConsumingCodec;

    impl Encoder for ConsumingCodec {
        type Item = Bytes;
        type Error = io::Error;

        fn encodev(&self, item: Bytes, dst: &mut BytePages) -> Result<(), Self::Error> {
            dst.append(item);
            Ok(())
        }
    }

    impl Decoder for ConsumingCodec {
        type Item = Bytes;
        type Error = io::Error;

        fn decode(&self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
            src.clear();
            Ok(None)
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
            let inner = State(io.get_ref());
            (Self::new(io, codec, Pipeline::new((), service)), inner)
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

        assert!(format!("{:?}", super::Flags::KA_TIMEOUT.clone()).contains("KA_TIMEOUT"));
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

        // service must be checked for readiness only once
        assert_eq!(counter.get(), 1);
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
                    .set_write_buf(16 * 1024, 1024, 16),
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
        assert_eq!(state.io().with_write_buf(|buf| buf.len()).unwrap(), 65536);

        client.remote_buffer_cap(10240);
        sleep(Millis(50)).await;
        assert_eq!(state.io().with_write_buf(|buf| buf.len()).unwrap(), 55296);

        client.remote_buffer_cap(48056);
        sleep(Millis(50)).await;
        assert_eq!(state.io().with_write_buf(|buf| buf.len()).unwrap(), 7240);

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
    async fn io_error_does_not_wait_for_pending_service_call() {
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);

        let (mut disp, _) = Dispatcher::debug(
            Io::from(server),
            BytesCodec,
            ntex_service::fn_service(move |msg: DispatchItem<BytesCodec>| {
                let stop = stop2.clone();
                async move {
                    match msg {
                        DispatchItem::Item(_) => {
                            std::future::pending::<Result<Option<Bytes>, ()>>().await
                        }
                        DispatchItem::Stop(Reason::Io(Some(_))) => {
                            stop.store(true, Relaxed);
                            Ok(None)
                        }
                        _ => Ok(None),
                    }
                }
            }),
        );

        client.write("request");
        assert!(lazy(|cx| Pin::new(&mut disp).poll(cx)).await.is_pending());

        client.read_error(io::Error::new(
            io::ErrorKind::ConnectionReset,
            "connection reset",
        ));
        timeout(Millis(1000), poll_fn(|cx| Pin::new(&mut disp).poll(cx)))
            .await
            .expect("dispatcher waited for pending service call")
            .unwrap();

        timeout(Millis(1000), async {
            while !stop.load(Relaxed) {
                sleep(Millis(10)).await;
            }
        })
        .await
        .expect("service did not receive I/O stop notification");
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
                .set_disconnect_timeout(Seconds(1))
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
                        DispatchItem::Stop(Reason::KeepAliveTimeout) => {
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
        assert!(state.0.is_stopping());
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
                        DispatchItem::Stop(Reason::KeepAliveTimeout) => {
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
        assert!(state.0.is_stopping());
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
                        DispatchItem::Stop(Reason::KeepAliveTimeout) => {
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
        assert!(!state.0.is_stopping());
        client.write("23");
        sleep(Millis(1000)).await;
        assert!(!state.0.is_stopping());
        client.write("4");
        sleep(Millis(2000)).await;

        // write side must be closed, dispatcher should fail with keep-alive
        assert!(state.0.is_stopping());
        assert!(client.is_closed());
        assert_eq!(&data.lock().unwrap().borrow()[..], &[0, 1]);
    }

    #[ntex::test]
    async fn read_timeout_starts_for_new_connection() {
        let timeout = Rc::new(Cell::new(false));
        let timeout2 = timeout.clone();
        let (client, server) = IoTest::create();
        let io = Io::new(
            server,
            SharedCfg::new("TEST").add(
                IoConfig::new()
                    .set_keepalive_timeout(Seconds::ZERO)
                    .set_frame_read_rate(Seconds(1), Seconds(2), 2),
            ),
        );

        let (mut disp, state) = Dispatcher::debug(
            io,
            BCodec(8),
            ntex_service::fn_service(move |msg: DispatchItem<BCodec>| {
                if matches!(msg, DispatchItem::Stop(Reason::ReadTimeout)) {
                    timeout2.set(true);
                }
                async { Ok::<_, ()>(None) }
            }),
        );

        assert!(matches!(disp.inner.read_state, ReadState::FirstFrame(_)));
        assert!(disp.inner.shared.contains(Flags::READ_TIMEOUT));
        state.io().notify_timeout();
        let _ = lazy(|cx| Pin::new(&mut disp).poll(cx)).await;

        assert!(timeout.get());
        client.close().await;
    }

    #[ntex::test]
    async fn keepalive_starts_for_new_connection_without_read_rate() {
        let timeout = Rc::new(Cell::new(false));
        let timeout2 = timeout.clone();
        let (client, server) = IoTest::create();
        let io = Io::new(
            server,
            SharedCfg::new("TEST").add(IoConfig::new().set_keepalive_timeout(Seconds::ONE)),
        );

        let (mut disp, state) = Dispatcher::debug(
            io,
            BCodec(8),
            ntex_service::fn_service(move |msg: DispatchItem<BCodec>| {
                if matches!(msg, DispatchItem::Stop(Reason::KeepAliveTimeout)) {
                    timeout2.set(true);
                }
                async { Ok::<_, ()>(None) }
            }),
        );

        assert_eq!(disp.inner.read_state, ReadState::Idle);
        assert!(lazy(|cx| Pin::new(&mut disp).poll(cx)).await.is_pending());
        assert!(disp.inner.shared.contains(Flags::KA_TIMEOUT));

        state.io().notify_timeout();
        let _ = lazy(|cx| Pin::new(&mut disp).poll(cx)).await;
        assert!(timeout.get());
        client.close().await;
    }

    #[ntex::test]
    async fn read_timeout_during_service_readiness_pause() {
        struct PendingReadyService {
            pending: Cell<bool>,
            reason: Rc<Cell<Option<bool>>>,
        }

        impl Service<(), DispatchItem<BCodec>> for PendingReadyService {
            type Res = Option<Bytes>;
            type Error = ();

            async fn ready(&self, ctx: Ctx<'_, Self>) -> Result<(), Self::Error> {
                ctx.poll_fn(|cx| {
                    if self.pending.replace(false) {
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    } else {
                        Poll::Ready(Ok(()))
                    }
                })
                .await
            }

            async fn call(
                &self,
                msg: DispatchItem<BCodec>,
                _: Ctx<'_, Self>,
            ) -> Result<Self::Res, Self::Error> {
                if let DispatchItem::Stop(reason) = msg {
                    self.reason.set(Some(matches!(reason, Reason::ReadTimeout)));
                }
                Ok(None)
            }
        }

        async fn check(keepalive: Seconds) {
            let reason = Rc::new(Cell::new(None));
            let (client, server) = IoTest::create();
            let io = Io::new(
                server,
                SharedCfg::new("TEST").add(
                    IoConfig::new()
                        .set_keepalive_timeout(keepalive)
                        .set_frame_read_rate(Seconds::ONE, Seconds(2), 2),
                ),
            );
            let (mut disp, state) = Dispatcher::debug(
                io,
                BCodec(8),
                PendingReadyService {
                    pending: Cell::new(true),
                    reason: reason.clone(),
                },
            );

            state.io().notify_timeout();
            assert!(lazy(|cx| Pin::new(&mut disp).poll(cx)).await.is_pending());
            let _ = lazy(|cx| Pin::new(&mut disp).poll(cx)).await;
            assert_eq!(reason.get(), Some(true));
            client.close().await;
        }

        check(Seconds::ZERO).await;
        check(Seconds::ONE).await;
    }

    #[ntex::test]
    async fn consumed_subsequent_frame_resumes_read_timer() {
        let (client, server) = IoTest::create();
        let io = Io::new(
            server,
            SharedCfg::new("TEST").add(
                IoConfig::new()
                    .set_keepalive_timeout(Seconds::ONE)
                    .set_frame_read_rate(Seconds::ONE, Seconds(2), 2),
            ),
        );
        let (mut disp, _) = Dispatcher::debug(
            io,
            ConsumingCodec,
            ntex_service::fn_service(|_: DispatchItem<ConsumingCodec>| async { Ok::<_, ()>(None) }),
        );

        disp.inner.update_timer(&Decoded {
            item: Some(Bytes::new()),
            remains: 0,
            consumed: 1,
        });
        assert_eq!(disp.inner.read_state, ReadState::Idle);

        disp.inner.update_timer(&Decoded {
            item: None,
            remains: 0,
            consumed: 3,
        });
        assert!(matches!(disp.inner.read_state, ReadState::ReadingFrame(_)));
        assert!(disp.inner.shared.contains(Flags::READ_TIMEOUT));

        disp.inner.shared.remove_flags(Flags::READ_TIMEOUT);
        disp.inner.shared.io.stop_timer();
        disp.inner.update_timer(&Decoded::<Bytes> {
            item: None,
            remains: 0,
            consumed: 0,
        });

        assert!(matches!(disp.inner.read_state, ReadState::ReadingFrame(_)));
        assert!(disp.inner.shared.contains(Flags::READ_TIMEOUT));
        assert!(!disp.inner.shared.contains(Flags::KA_TIMEOUT));
        client.close().await;
    }

    #[ntex::test]
    async fn read_timeout_tracks_consumed_bytes_and_stalls() {
        let timeout = Rc::new(Cell::new(false));
        let timeout2 = timeout.clone();
        let (client, server) = IoTest::create();
        let io = Io::new(
            server,
            SharedCfg::new("TEST").add(
                IoConfig::new()
                    .set_keepalive_timeout(Seconds::ZERO)
                    .set_frame_read_rate(Seconds(1), Seconds::ZERO, 2),
            ),
        );

        let (mut disp, state) = Dispatcher::debug(
            io,
            ConsumingCodec,
            ntex_service::fn_service(move |msg: DispatchItem<ConsumingCodec>| {
                if matches!(msg, DispatchItem::Stop(Reason::ReadTimeout)) {
                    timeout2.set(true);
                }
                async { Ok::<_, ()>(None) }
            }),
        );

        client.write("123");
        sleep(Millis(25)).await;
        assert!(lazy(|cx| Pin::new(&mut disp).poll(cx)).await.is_pending());
        let progress = disp.inner.read_state.progress().unwrap();
        assert_eq!(progress.consumed, 3);
        assert_eq!(progress.remains, 0);

        state.io().notify_timeout();
        assert!(lazy(|cx| Pin::new(&mut disp).poll(cx)).await.is_pending());
        assert!(!timeout.get());
        assert_eq!(disp.inner.read_state.progress().unwrap().consumed, 0);

        state.io().notify_timeout();
        let _ = lazy(|cx| Pin::new(&mut disp).poll(cx)).await;
        assert!(timeout.get());
        client.close().await;
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
        assert!(state.0.is_stopping());
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
                SharedCfg::new("DBG").add(IoConfig::new().set_write_buf(2, 1, 128)),
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
}
