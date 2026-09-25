//! HTTP/1 protocol dispatcher
use std::{future, io, mem, pin::Pin, rc::Rc, task::Context, task::Poll, task::ready};

use crate::io::{Decoded, Filter, Io, IoStatusUpdate, RecvError};
use crate::service::pipeline::{Pipeline, PipelineCall};
use crate::{channel::bstream, util::Either, util::clone_io_error};

use crate::http::body::{BodySize, MessageBody, ResponseBody};
use crate::http::error::{DispatchError, PayloadError, ResponseError};
use crate::http::message::CurrentIo;
use crate::http::{self, config::DispatcherConfig, request::Request, response::Response};

use super::control::{Control, ControlAck, ControlResult, ServiceDisconnectReason};
use super::decoder::{PayloadDecoder, PayloadItem, PayloadType};
use super::{Message, ProtocolError, codec::Codec, timer::Timer, timer::Timers};

pin_project_lite::pin_project! {
    /// Dispatcher for HTTP/1.1 protocol
    pub struct Dispatcher<F, B, Err> {
        st: State<F, B, Err>,
        inner: DispatcherInner<F, B, Err>,
    }
}

#[derive(Debug)]
enum State<F, B, Err> {
    CallPublish {
        fut: PipelineCall<Request, Response<B>, Err>,
    },
    CallControl {
        fut: PipelineCall<Control<F, Err>, ControlAck<F>, DispatchError>,
    },
    ReadRequest,
    ReadPayload,
    SendPayload {
        body: ResponseBody<B>,
    },
    Stop,
}

/// Control service disconnect state
#[derive(Debug)]
enum Disconnect {
    None,
    /// Disconnect reason to send once the current response is written
    Pending(ServiceDisconnectReason),
    /// Disconnect control message has been sent
    Sent,
}

struct DispatcherInner<F, B, Err> {
    io: Rc<Io<F>>,
    codec: Codec,
    timers: Timers,
    config: DispatcherConfig,
    disconnect: Disconnect,
    service: Pipeline<Request, Response<B>, Err>,
    control: Pipeline<Control<F, Err>, ControlAck<F>, DispatchError>,
    payload: Option<(PayloadDecoder, bstream::Sender<PayloadError>)>,
    pending_payload_error: Option<Either<ProtocolError, Option<io::Error>>>,
}

impl<F, B, Err> Dispatcher<F, B, Err>
where
    F: Filter,
    B: MessageBody,
    Err: ResponseError + 'static,
{
    /// Construct new `Dispatcher` instance with outgoing messages stream.
    pub(in crate::http) fn new(
        id: usize,
        io: Io<F>,
        service: Pipeline<Request, Response<B>, Err>,
        control: Pipeline<Control<F, Err>, ControlAck<F>, DispatchError>,
        config: DispatcherConfig,
    ) -> Self {
        let codec = Codec::new(id, io.shared().get());

        // slow-request timer
        let timers = Timers::new(&io, codec.cfg.headers_read_rate);

        Dispatcher {
            st: State::ReadRequest,
            inner: DispatcherInner {
                codec,
                timers,
                config,
                service,
                control,
                io: Rc::new(io),
                payload: None,
                pending_payload_error: None,
                disconnect: Disconnect::None,
            },
        }
    }
}

impl<F, B, Err> future::Future for Dispatcher<F, B, Err>
where
    F: Filter,
    B: MessageBody,
    Err: ResponseError + 'static,
{
    type Output = Result<(), DispatchError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let inner = this.inner;

        loop {
            *this.st = match this.st {
                // handle publish service responses
                State::CallPublish { fut } => match Pin::new(fut).poll(cx) {
                    Poll::Ready(Ok(res)) => {
                        let (res, body) = res.into_parts();
                        inner.send_response(res, body)
                    }
                    Poll::Ready(Err(err)) => inner.ctl_error(err),
                    Poll::Pending => {
                        // state changed because of error
                        // otherwise .poll_request() returns Poll::Pending
                        let st = ready!(inner.poll_request(cx));

                        // spawn current publish future to runtime
                        // so it could complete error handling
                        if let State::CallPublish { fut } =
                            mem::replace(&mut *this.st, State::ReadRequest)
                        {
                            crate::rt::spawn(fut);
                        }
                        st
                    }
                },
                // handle control service responses
                State::CallControl { fut } => {
                    let result = match Pin::new(fut).poll(cx) {
                        Poll::Ready(result) => result,
                        Poll::Pending => {
                            // Check for payload errors while waiting for the control
                            // service, but preserve the in-flight control call.
                            if inner.pending_payload_error.is_none()
                                && let Poll::Ready(Err(err)) =
                                    inner.poll_request_payload_inner::<F>(None, cx)
                            {
                                inner.pending_payload_error = Some(err);
                            }
                            return Poll::Pending;
                        }
                    };

                    match result {
                        Ok(ControlAck { result }) => match result {
                            ControlResult::Publish(req) => inner.publish(req),
                            ControlResult::Response(res, body)
                            | ControlResult::Error(res, body)
                            | ControlResult::ProtocolError(res, body) => {
                                inner.send_response(res, body.into())
                            }
                            ControlResult::Continue(req) => {
                                let result =
                                    inner.io.encode_slice(b"HTTP/1.1 100 Continue\r\n\r\n");
                                if let Err(err) = result {
                                    *this.st = inner.ctl_peer_gone(Some(err));
                                    continue;
                                }
                                inner.start_payload_timer();
                                if req.upgrade() {
                                    inner.ctl_upgrade(req)
                                } else {
                                    inner.publish(req)
                                }
                            }
                            ControlResult::Expect(req) => inner.control(Control::expect(req)),
                            ControlResult::ExpectFailed(res, body) => {
                                inner.set_disconnect(ServiceDisconnectReason::ExpectFailed);
                                inner.send_response(res, body.into())
                            }
                            ControlResult::Upgrade(req) => inner.ctl_upgrade(req),
                            ControlResult::UpgradeAck(req) => {
                                inner.set_disconnect(ServiceDisconnectReason::UpgradeHandled);
                                inner.publish(req)
                            }
                            ControlResult::UpgradeHandled => {
                                inner.ctl_svc_disconnect(ServiceDisconnectReason::UpgradeHandled)
                            }
                            ControlResult::UpgradeFailed(res, body) => {
                                inner.set_disconnect(ServiceDisconnectReason::UpgradeFailed);
                                inner.send_response(res, body.into())
                            }
                            ControlResult::Stop => inner.stop(),
                            ControlResult::Connect(_) => unreachable!(),
                        },
                        Err(err) => {
                            log::error!("{}: Control plain error: {}", inner.io.tag(), err);
                            return Poll::Ready(Err(err));
                        }
                    }
                }
                // read request and call service
                State::ReadRequest => {
                    if let Some(st) = inner.check_disconnect() {
                        st
                    } else {
                        ready!(inner.poll_read_request(cx))
                    }
                }
                // consume request's payload
                State::ReadPayload => {
                    let result = inner.poll_request_payload(cx);
                    if let Some(st) = inner.check_disconnect() {
                        st
                    } else {
                        ready!(result).unwrap_or(State::ReadRequest)
                    }
                }
                // send response body
                State::SendPayload { body } => {
                    ready!(inner.poll_send_payload(cx, body))
                }
                // shutdown io
                State::Stop => {
                    let _ = ready!(inner.io.poll_shutdown(cx));
                    return Poll::Ready(Ok(()));
                }
            }
        }
    }
}

impl<F, B, Err> DispatcherInner<F, B, Err>
where
    F: Filter,
    B: MessageBody,
    Err: ResponseError + 'static,
{
    fn poll_read_request(&mut self, cx: &mut Context<'_>) -> Poll<State<F, B, Err>> {
        // stop dispatcher
        if self.config.is_shutdown() {
            log::trace!("{}: Service is shutting down", self.io.tag());
            return Poll::Ready(self.ctl_svc_disconnect(ServiceDisconnectReason::Shutdown));
        }

        log::trace!("{}: Trying to read http message", self.io.tag());
        self.release_write_timer();

        let buffered = self.io.with_read_dst(|buf| buf.len()) as u32;
        self.timers.headers_buffered(buffered);

        let result = match self.io.poll_recv_decode(&self.codec, cx) {
            Ok(decoded) => {
                if let Some(st) = self.update_hdrs_timer(&decoded) {
                    return Poll::Ready(st);
                }
                if let Some(item) = decoded.item {
                    Ok(item)
                } else {
                    return Poll::Pending;
                }
            }
            Err(err) => Err(err),
        };

        // decode incoming bytes stream
        let st = match result {
            Ok((mut req, pl)) => {
                log::trace!(
                    "{}: Http message is received: {:?} and payload {:?}",
                    self.io.tag(),
                    req,
                    pl
                );
                req.head_mut().io = CurrentIo::Ref(self.io.get_ref());

                // configure request payload
                match pl {
                    PayloadType::None => (),
                    PayloadType::Payload(decoder) | PayloadType::Stream(decoder) => {
                        let (ps, pl) = bstream::channel();
                        req.replace_payload(http::Payload::H1(pl));
                        self.payload = Some((decoder, ps));

                        // the client does not send the body before `100 Continue`
                        if !req.head().expect() {
                            self.start_payload_timer();
                        }
                    }
                }
                self.control(Control::request(req))
            }
            Err(RecvError::WriteBackpressure) => {
                if let Err(err) = ready!(self.poll_flush_timed(cx)) {
                    log::trace!("{}: Peer is gone with {:?}", self.io.tag(), err);
                    self.ctl_peer_gone(Some(err))
                } else {
                    ready!(self.poll_read_request(cx))
                }
            }
            Err(RecvError::Decoder(err)) => {
                // Malformed requests, respond with 400
                log::trace!("{}: Malformed request: {:?}", self.io.tag(), err);
                self.ctl_proto_err(err.into())
            }
            Err(RecvError::PeerGone(err)) => {
                log::trace!("{}: Peer is gone with {:?}", self.io.tag(), err);
                self.ctl_peer_gone(err)
            }
            Err(RecvError::KeepAlive) => {
                if self.timers.active.is_write() {
                    if self.io.is_wr_backpressure() {
                        log::trace!("{}: Write backpressure timeout", self.io.tag());
                        self.ctl_peer_gone(Some(write_timeout_error()))
                    } else {
                        self.timers.stop_write(&self.io);
                        ready!(self.poll_read_request(cx))
                    }
                } else if self.timers.active == Timer::Headers {
                    if let Err(err) = self.handle_timeout() {
                        log::trace!("{}: Slow request timeout", self.io.tag());
                        self.ctl_proto_err(err)
                    } else {
                        ready!(self.poll_read_request(cx))
                    }
                } else if self.codec.is_reading_hdrs() && self.codec.cfg.headers_read_rate.is_some()
                {
                    // a partial request head wins over keep-alive or client timeout
                    let remains = self.io.with_read_dst(|buf| buf.len()) as u32;
                    self.start_headers_timer(buffered, remains);
                    ready!(self.poll_read_request(cx))
                } else if self.timers.active == Timer::ClientTimeout {
                    log::trace!("{}: Client timeout, no request", self.io.tag());
                    self.ctl_proto_err(ProtocolError::SlowRequestTimeout)
                } else {
                    log::trace!("{}: Keep-alive timeout, close connection", self.io.tag());
                    self.ctl_keepalive(true)
                }
            }
        };

        Poll::Ready(st)
    }

    fn send_response(&mut self, mut msg: Response<()>, body: ResponseBody<B>) -> State<F, B, Err> {
        log::trace!(
            "{}: Sending response: {:?} body: {:?}",
            self.io.tag(),
            msg,
            body.size()
        );
        // close connection if payload stream is dropped and not consumed
        if let Some((_pl, snd)) = &self.payload
            && snd.is_closed()
        {
            msg.head_mut()
                .set_connection_type(http::ConnectionType::Close);
        }

        // we don't need to process responses if socket is disconnected
        // but we still want to handle requests with app service
        // so we skip response processing for dropped connection
        if self.io.is_active() {
            let result = self
                .io
                .encode(Message::Item((msg, body.size())), &self.codec)
                .inspect_err(|_| {
                    if let Some(ref mut payload) = self.payload {
                        payload.1.set_error(PayloadError::Incomplete(None));
                    }
                });

            match result {
                Ok(()) => match body.size() {
                    BodySize::None | BodySize::Empty => {
                        if let Some(st) = self.check_disconnect() {
                            st
                        } else if self.payload.is_some() {
                            self.read_payload()
                        } else {
                            State::ReadRequest
                        }
                    }
                    _ => State::SendPayload { body },
                },
                Err(err) => self.ctl_proto_err(err.into()),
            }
        } else {
            self.ctl_peer_gone(None)
        }
    }

    fn poll_send_payload(
        &mut self,
        cx: &mut Context<'_>,
        body: &mut ResponseBody<B>,
    ) -> Poll<State<F, B, Err>> {
        let payload_err = if let Some(err) = self.pending_payload_error.take() {
            Some(err)
        } else if !self.io.is_active() {
            return Poll::Ready(self.ctl_peer_gone(None));
        } else if !matches!(self.disconnect, Disconnect::Pending(_))
            && let Poll::Ready(Err(err)) = self.poll_request_payload_inner::<F>(None, cx)
        {
            Some(err)
        } else {
            None
        };
        match payload_err {
            // peer is gone or write timeout, the response cannot be completed
            Some(Either::Right(err)) => return Poll::Ready(self.ctl_peer_gone(err)),
            // the response head is sent, an error response is not possible,
            // finish the response and then stop
            Some(Either::Left(err)) => {
                log::trace!("{}: Request payload error: {:?}", self.io.tag(), err);
                self.disconnect = Disconnect::Sent;
            }
            None => (),
        }
        loop {
            if let Err(err) = ready!(self.poll_flush_timed(cx))
                && err.kind() == io::ErrorKind::TimedOut
            {
                log::trace!("{}: Write backpressure timeout", self.io.tag());
                self.set_payload_error(PayloadError::Io(write_timeout_error()));
                return Poll::Ready(self.ctl_peer_gone(Some(err)));
            }
            let item = ready!(body.poll_next_chunk(cx));

            let st = match item {
                Some(Ok(item)) => {
                    log::trace!("{}: Got response chunk: {:?}", self.io.tag(), item.len());
                    match self.io.encode(Message::Chunk(Some(item)), &self.codec) {
                        Ok(()) => continue,
                        Err(err) => self.ctl_proto_err(err.into()),
                    }
                }
                None => {
                    log::trace!(
                        "{}: Response payload eof {:?}",
                        self.io.tag(),
                        self.disconnect
                    );
                    if let Err(err) = self.io.encode(Message::Chunk(None), &self.codec) {
                        self.ctl_proto_err(err.into())
                    } else if let Some(st) = self.check_disconnect() {
                        st
                    } else if self.payload.is_some() {
                        self.read_payload()
                    } else {
                        State::ReadRequest
                    }
                }
                Some(Err(err)) => {
                    log::trace!(
                        "{}: Error during response body poll: {:?}",
                        self.io.tag(),
                        err
                    );
                    self.ctl_proto_err(ProtocolError::ResponsePayload(err))
                }
            };
            return Poll::Ready(st);
        }
    }

    /// we might need to read more data into a request payload
    /// (ie service future can wait for payload data)
    fn poll_request(&mut self, cx: &mut Context<'_>) -> Poll<State<F, B, Err>> {
        if self.payload.is_some() {
            if let Some(st) = ready!(self.poll_request_payload(cx)) {
                Poll::Ready(st)
            } else {
                Poll::Pending
            }
        } else if let Some(st) = self.take_payload_error() {
            Poll::Ready(st)
        } else {
            // check for io changes, it could be close while waiting for service call
            let Poll::Ready(status) = self.io.poll_status_update(cx) else {
                // write backpressure can be disabled by the status update
                self.release_write_timer();
                return Poll::Pending;
            };
            match status {
                IoStatusUpdate::KeepAlive
                    if self.timers.active.is_write() && self.io.is_wr_backpressure() =>
                {
                    log::trace!("{}: Write backpressure timeout", self.io.tag());
                    Poll::Ready(self.ctl_peer_gone(Some(write_timeout_error())))
                }
                IoStatusUpdate::KeepAlive => {
                    self.timers.stop_write(&self.io);
                    Poll::Pending
                }
                IoStatusUpdate::WriteBackpressure => {
                    self.start_write_timer();
                    Poll::Pending
                }
                IoStatusUpdate::PeerGone(e) => Poll::Ready(self.ctl_peer_gone(e)),
            }
        }
    }

    fn set_payload_error(&mut self, err: PayloadError) {
        if let Some(ref mut payload) = self.payload {
            payload.1.set_error(err);
        }
    }

    /// Process request's payload
    fn poll_request_payload(&mut self, cx: &mut Context<'_>) -> Poll<Option<State<F, B, Err>>> {
        if let Some(st) = self.take_payload_error() {
            Poll::Ready(Some(st))
        } else if let Err(err) = ready!(self.poll_request_payload_inner::<F>(None, cx)) {
            Poll::Ready(Some(match err {
                Either::Left(e) => self.ctl_proto_err(e),
                Either::Right(e) => self.ctl_peer_gone(e),
            }))
        } else {
            Poll::Ready(None)
        }
    }

    #[allow(clippy::map_unwrap_or)]
    /// Process request's payload
    fn poll_request_payload_inner<Fi>(
        &mut self,
        io: Option<&Io<Fi>>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), Either<ProtocolError, Option<io::Error>>>> {
        // check if payload data is required
        if self.payload.is_none() {
            return Poll::Ready(Ok(()));
        }

        match self.payload.as_ref().unwrap().1.poll_ready(cx) {
            Poll::Ready(bstream::Status::Ready) => {
                // read request payload
                let mut updated = false;
                self.release_write_timer();
                loop {
                    let buffered = if self.timers.active == Timer::Payload {
                        Some(
                            io.map(|io| io.with_read_dst(|buf| buf.len()))
                                .unwrap_or_else(|| self.io.with_read_dst(|buf| buf.len())),
                        )
                    } else {
                        None
                    };
                    let recv_result = io
                        .map(|io| io.poll_recv_decode(&self.payload.as_ref().unwrap().0, cx))
                        .unwrap_or_else(|| {
                            self.io
                                .poll_recv_decode(&self.payload.as_ref().unwrap().0, cx)
                        });

                    let res = match recv_result {
                        Ok(decoded) => {
                            self.update_payload_timer(&decoded);
                            if let Some(item) = decoded.item {
                                updated = true;
                                Ok(item)
                            } else {
                                break;
                            }
                        }
                        Err(err) => Err(err),
                    };

                    match res {
                        Ok(PayloadItem::Chunk(chunk)) => {
                            self.payload.as_mut().unwrap().1.feed_data(chunk);
                        }
                        Ok(PayloadItem::Eof) => {
                            self.timers.payload_done(&self.io);
                            self.payload.as_mut().unwrap().1.feed_eof();
                            self.payload = None;
                            break;
                        }
                        Err(err) => {
                            let err = match err {
                                RecvError::WriteBackpressure => {
                                    let flush_result = if let Some(io) = io {
                                        io.poll_flush(cx, false)
                                    } else {
                                        self.poll_flush_timed(cx)
                                    };

                                    match flush_result {
                                        Poll::Ready(Ok(())) => continue,
                                        Poll::Ready(Err(err)) => {
                                            if err.kind() == io::ErrorKind::TimedOut {
                                                self.set_payload_error(PayloadError::Io(
                                                    write_timeout_error(),
                                                ));
                                            }
                                            Either::Right(Some(err))
                                        }
                                        Poll::Pending => {
                                            self.pause_payload_timer();
                                            break;
                                        }
                                    }
                                }
                                RecvError::KeepAlive if self.timers.active.is_write() => {
                                    if self.io.is_wr_backpressure() {
                                        self.set_payload_error(PayloadError::Io(
                                            write_timeout_error(),
                                        ));
                                        Either::Right(Some(write_timeout_error()))
                                    } else {
                                        self.timers.stop_write(&self.io);
                                        continue;
                                    }
                                }
                                RecvError::KeepAlive => {
                                    if let Some(buffered) = buffered {
                                        let remains = io
                                            .map(|io| io.with_read_dst(|buf| buf.len()))
                                            .unwrap_or_else(|| {
                                                self.io.with_read_dst(|buf| buf.len())
                                            });
                                        let p = &mut self.timers.progress;
                                        p.consumed =
                                            p.consumed.saturating_add(
                                                buffered.saturating_sub(remains) as u32,
                                            );
                                    }
                                    if let Err(err) = self.handle_timeout() {
                                        Either::Left(err)
                                    } else {
                                        continue;
                                    }
                                }
                                RecvError::PeerGone(err) => {
                                    self.set_payload_error(PayloadError::Incomplete(
                                        err.as_ref().map(clone_io_error),
                                    ));
                                    Either::Right(err)
                                }
                                RecvError::Decoder(e) => {
                                    self.set_payload_error(PayloadError::Decode(e));
                                    Either::Left(ProtocolError::Decode(e))
                                }
                            };
                            return Poll::Ready(Err(err));
                        }
                    }
                }
                if updated {
                    Poll::Ready(Ok(()))
                } else {
                    Poll::Pending
                }
            }
            Poll::Pending => {
                self.pause_payload_timer();
                Poll::Pending
            }
            Poll::Ready(bstream::Status::Dropped | bstream::Status::Eof) => {
                // service call is not interested in payload
                // wait until future completes and then close
                // connection
                self.payload = None;
                self.set_disconnect(ServiceDisconnectReason::PayloadDropped);
                Poll::Pending
            }
        }
    }

    /// Flushes output during write backpressure, bounded by the write
    /// timeout.
    fn poll_flush_timed(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        loop {
            if let Poll::Ready(res) = self.io.poll_flush(cx, false) {
                self.timers.stop_write(&self.io);
                return Poll::Ready(res);
            }
            self.start_write_timer();
            if !self.timers.active.is_write() {
                return Poll::Pending;
            }
            // the timer is reported through the status update
            match self.io.poll_status_update(cx) {
                Poll::Ready(IoStatusUpdate::KeepAlive) => {
                    return Poll::Ready(Err(write_timeout_error()));
                }
                Poll::Pending if !self.io.is_wr_backpressure() => (),
                _ => return Poll::Pending,
            }
        }
    }

    fn start_write_timer(&mut self) {
        self.timers
            .start_write(&self.io, self.codec.cfg.write_timeout);
    }

    /// Stops the write timer if write backpressure has been disabled.
    fn release_write_timer(&mut self) {
        if self.timers.active.is_write() && !self.io.is_wr_backpressure() {
            self.timers.stop_write(&self.io);
        }
    }

    fn pause_payload_timer(&mut self) {
        self.timers.pause_payload(&self.io);
    }

    fn handle_timeout(&mut self) -> Result<(), ProtocolError> {
        // check read rate
        let (cfg, payload) = match self.timers.active {
            Timer::Headers => (&self.codec.cfg.headers_read_rate, false),
            Timer::Payload => (&self.codec.cfg.payload_read_rate, true),
            _ => return Ok(()),
        };
        let Some(cfg) = *cfg else {
            return Ok(());
        };
        if self.timers.extend(&self.io, cfg) {
            return Ok(());
        }

        log::trace!(
            "{}: Timeout during reading, {:?}",
            self.io.tag(),
            self.timers.active
        );
        if payload {
            self.set_payload_error(PayloadError::Io(io::Error::new(
                io::ErrorKind::TimedOut,
                "Payload read timeout",
            )));
            Err(ProtocolError::SlowPayloadTimeout)
        } else {
            Err(ProtocolError::SlowRequestTimeout)
        }
    }

    fn update_hdrs_timer(
        &mut self,
        decoded: &Decoded<(Request, PayloadType)>,
    ) -> Option<State<F, B, Err>> {
        // got parsed frame
        if decoded.item.is_some() {
            self.timers.reset(&self.io);
        } else if self.timers.active == Timer::Headers {
            // received new data but not enough for parsing complete frame
            self.timers.progress.remains = decoded.remains as u32;
        } else if self.timers.active == Timer::ClientTimeout {
            // only the headers read rate bounds the first request, it starts
            // with the first received byte
            if (self.codec.is_reading_hdrs() || decoded.remains != 0)
                && self.codec.cfg.headers_read_rate.is_some()
            {
                self.start_headers_timer(
                    (decoded.consumed as u32).saturating_add(decoded.remains as u32),
                    decoded.remains as u32,
                );
            }
        } else if self.codec.is_reading_hdrs() || decoded.remains != 0 {
            // partial request head, without a headers read rate the
            // keep-alive timer bounds it
            if self.codec.cfg.headers_read_rate.is_some() {
                self.start_headers_timer(
                    (decoded.consumed as u32).saturating_add(decoded.remains as u32),
                    decoded.remains as u32,
                );
            } else if self.codec.cfg.ka_enabled {
                self.timers
                    .start_keepalive(&self.io, self.codec.cfg.keep_alive);
            }
        } else if self.codec.keepalive() {
            // no new data, start keep-alive timer
            if self.codec.cfg.ka_enabled {
                self.timers
                    .start_keepalive(&self.io, self.codec.cfg.keep_alive);
            }
        } else {
            self.io.close();
            return Some(self.ctl_keepalive(false));
        }
        None
    }

    fn start_headers_timer(&mut self, consumed: u32, remains: u32) {
        self.timers.start_headers(
            &self.io,
            self.codec.cfg.headers_read_rate,
            consumed,
            remains,
        );
    }

    fn update_payload_timer(&mut self, decoded: &Decoded<PayloadItem>) {
        self.timers
            .payload_decoded(&self.io, decoded.consumed as u32);
    }

    /// Starts payload timing for the current request if it is not started
    /// yet, an expectation can be handled without `100 Continue`.
    fn start_payload_timer(&mut self) {
        if self.payload.is_some() && matches!(self.timers.active, Timer::Stopped | Timer::Write) {
            self.timers
                .start_payload(&self.io, self.codec.cfg.payload_read_rate);
        }
    }

    /// Switches to reading the rest of the request payload after the
    /// response is sent.
    fn read_payload(&mut self) -> State<F, B, Err> {
        self.start_payload_timer();
        State::ReadPayload
    }

    fn publish(&mut self, req: Request) -> State<F, B, Err> {
        // payload failed while waiting for the control service
        if let Some(st) = self.take_payload_error() {
            return st;
        }
        self.start_payload_timer();
        State::CallPublish {
            fut: self.service.call_nowait(req),
        }
    }

    fn take_payload_error(&mut self) -> Option<State<F, B, Err>> {
        self.pending_payload_error.take().map(|err| match err {
            Either::Left(err) => self.ctl_proto_err(err),
            Either::Right(err) => self.ctl_peer_gone(err),
        })
    }

    fn control(&self, req: Control<F, Err>) -> State<F, B, Err> {
        State::CallControl {
            fut: self.control.call_nowait(req),
        }
    }

    fn ctl_upgrade(&mut self, req: Request) -> State<F, B, Err> {
        self.codec.reset_upgrade();
        self.control(Control::upgrade(req, self.io.clone(), self.codec.clone()))
    }

    fn ctl_keepalive(&mut self, enabled: bool) -> State<F, B, Err> {
        self.ctl_disconnect(Control::keepalive(enabled))
    }

    fn ctl_error(&mut self, err: Err) -> State<F, B, Err> {
        self.ctl_disconnect(Control::err(err))
    }

    fn ctl_proto_err(&mut self, err: ProtocolError) -> State<F, B, Err> {
        self.ctl_disconnect(Control::proto_err(err))
    }

    fn ctl_peer_gone(&mut self, err: Option<io::Error>) -> State<F, B, Err> {
        self.ctl_disconnect(Control::peer_gone(err))
    }

    fn ctl_svc_disconnect(&mut self, reason: ServiceDisconnectReason) -> State<F, B, Err> {
        self.ctl_disconnect(Control::svc_disconnect(reason))
    }

    /// Records a disconnect reason, unless a disconnect has been sent.
    fn set_disconnect(&mut self, reason: ServiceDisconnectReason) {
        if !matches!(self.disconnect, Disconnect::Sent) {
            self.disconnect = Disconnect::Pending(reason);
        }
    }

    fn ctl_disconnect(&mut self, req: Control<F, Err>) -> State<F, B, Err> {
        if matches!(
            mem::replace(&mut self.disconnect, Disconnect::Sent),
            Disconnect::Sent
        ) {
            self.stop()
        } else {
            State::CallControl {
                fut: self.control.call_nowait(req),
            }
        }
    }

    fn check_disconnect(&mut self) -> Option<State<F, B, Err>> {
        match mem::replace(&mut self.disconnect, Disconnect::Sent) {
            Disconnect::None => {
                self.disconnect = Disconnect::None;
                None
            }
            Disconnect::Pending(reason) => Some(State::CallControl {
                fut: self.control.call_nowait(Control::svc_disconnect(reason)),
            }),
            Disconnect::Sent => Some(self.stop()),
        }
    }

    fn stop(&mut self) -> State<F, B, Err> {
        log::debug!("{}: Dispatcher is stopped", self.io.tag());

        self.timers.stop(&self.io);
        State::Stop
    }
}

fn write_timeout_error() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "Write backpressure timeout")
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::{error, future::Future, future::poll_fn, sync::Arc};

    use rand::Rng;

    use super::*;
    use crate::http::config::HttpServiceConfig;
    use crate::http::h1::{DefaultControlService, control::Reason};
    use crate::http::{KeepAlive, ResponseHead, StatusCode, body};
    use crate::io::{self as nio, Base, testing::IoTest};
    use crate::service::{IntoService, Service, cfg::SharedCfg, fn_service};
    use crate::time::{Millis, Seconds, sleep, timeout};
    use crate::util::{Bytes, BytesMut, lazy, stream_recv};
    use crate::{client::ClientCodec, codec::Decoder};

    const BUFFER_SIZE: usize = 32_768;

    #[crate::rt_test]
    async fn test_payload_timer_resume_preserves_maximum() {
        let (_client, server) = IoTest::create();
        let config: SharedCfg = SharedCfg::new("SVC")
            .add(HttpServiceConfig::new().set_payload_read_rate(Seconds(10), Seconds(15), 1))
            .into();

        let mut h1 = Dispatcher::new(
            0,
            nio::Io::new(server, config),
            Pipeline::new(
                (),
                fn_service(async |_| Ok::<_, io::Error>(Response::Ok().build())),
            ),
            Pipeline::new((), DefaultControlService),
            DispatcherConfig::default(),
        );
        let decoded = Decoded {
            item: None,
            remains: 0,
            consumed: 2,
        };

        h1.inner.timers.stop(&h1.inner.io);
        h1.inner.payload = Some((
            PayloadDecoder::length(4),
            bstream::channel::<PayloadError>().0,
        ));
        h1.inner.start_payload_timer();
        h1.inner.update_payload_timer(&decoded);
        assert_eq!(h1.inner.timers.progress.max_timeout, Seconds(5));
        assert!(h1.inner.handle_timeout().is_ok());
        assert_eq!(h1.inner.timers.progress.max_timeout, Seconds::ZERO);
        assert_eq!(h1.inner.io.timer_handle().remains(), Seconds(5));

        h1.inner.pause_payload_timer();
        assert_eq!(h1.inner.timers.active, Timer::PayloadPaused);
        assert_eq!(h1.inner.timers.progress.period, Seconds(5));
        assert_eq!(h1.inner.timers.progress.max_timeout, Seconds::ZERO);

        h1.inner.update_payload_timer(&decoded);
        assert_ne!(h1.inner.timers.active, Timer::PayloadPaused);
        assert_eq!(h1.inner.timers.progress.max_timeout, Seconds::ZERO);
        assert_eq!(h1.inner.io.timer_handle().remains(), Seconds(5));
    }

    /// A sent disconnect absorbs pending and later disconnect reasons.
    #[crate::rt_test]
    async fn test_disconnect_state() {
        let (_client, server) = IoTest::create();
        let config: SharedCfg = SharedCfg::new("SVC").into();
        let mut h1 = Dispatcher::new(
            0,
            nio::Io::new(server, config),
            Pipeline::new(
                (),
                fn_service(async |_| Ok::<_, io::Error>(Response::Ok().build())),
            ),
            Pipeline::new((), DefaultControlService),
            DispatcherConfig::default(),
        );

        assert!(h1.inner.check_disconnect().is_none());
        assert!(matches!(h1.inner.disconnect, Disconnect::None));

        h1.inner
            .set_disconnect(ServiceDisconnectReason::ExpectFailed);
        assert!(matches!(
            h1.inner.disconnect,
            Disconnect::Pending(ServiceDisconnectReason::ExpectFailed)
        ));
        assert!(matches!(
            h1.inner.ctl_peer_gone(None),
            State::CallControl { .. }
        ));
        assert!(matches!(h1.inner.disconnect, Disconnect::Sent));

        h1.inner
            .set_disconnect(ServiceDisconnectReason::PayloadDropped);
        assert!(matches!(h1.inner.disconnect, Disconnect::Sent));
        assert!(matches!(h1.inner.check_disconnect(), Some(State::Stop)));
        assert!(matches!(h1.inner.ctl_peer_gone(None), State::Stop));
    }

    fn exhausted_payload_h1(
        server: IoTest,
        rate: u32,
    ) -> (
        Dispatcher<Base, body::Body, io::Error>,
        bstream::Receiver<PayloadError>,
    ) {
        let config: SharedCfg = SharedCfg::new("SVC")
            .add(HttpServiceConfig::new().set_payload_read_rate(Seconds(1), Seconds(5), rate))
            .into();
        let mut h1 = Dispatcher::new(
            0,
            nio::Io::new(server, config),
            Pipeline::new(
                (),
                fn_service(async |_| Ok::<_, io::Error>(Response::Ok().build())),
            ),
            Pipeline::new((), DefaultControlService),
            DispatcherConfig::default(),
        );
        let (tx, rx) = bstream::channel::<PayloadError>();
        h1.inner.payload = Some((PayloadDecoder::length(4), tx));
        h1.inner.timers.stop(&h1.inner.io);
        h1.inner.timers.active = Timer::PayloadPaused;
        h1.inner.timers.progress.max_timeout = Seconds::ZERO;
        (h1, rx)
    }

    /// Resuming without budget left decodes the buffered payload first.
    #[crate::rt_test]
    async fn test_exhausted_payload_budget_decodes_buffered_data() {
        let (client, server) = IoTest::create();
        let (mut h1, mut rx) = exhausted_payload_h1(server, 1);

        client.write("test");
        sleep(Millis(50)).await;
        let res = lazy(|cx| h1.inner.poll_request_payload_inner::<Base>(None, cx)).await;
        assert!(matches!(res, Poll::Ready(Ok(()))));
        assert!(h1.inner.payload.is_none());
        assert_eq!(
            stream_recv(&mut rx).await.unwrap().unwrap(),
            Bytes::from("test")
        );
        assert!(stream_recv(&mut rx).await.is_none());

        let (client, server) = IoTest::create();
        let (mut h1, mut rx) = exhausted_payload_h1(server, 1);

        client.write("te");
        sleep(Millis(50)).await;
        let res = lazy(|cx| h1.inner.poll_request_payload_inner::<Base>(None, cx)).await;
        assert!(matches!(
            res,
            Poll::Ready(Err(Either::Left(ProtocolError::SlowPayloadTimeout)))
        ));
        assert_eq!(
            stream_recv(&mut rx).await.unwrap().unwrap(),
            Bytes::from("te")
        );
        assert!(stream_recv(&mut rx).await.unwrap().is_err());
    }

    /// Resuming continues the interrupted period instead of starting a new one.
    #[crate::rt_test]
    async fn test_payload_resume_continues_period() {
        let (client, server) = IoTest::create();
        let (mut h1, _rx) = exhausted_payload_h1(server, 1);
        h1.inner.timers.progress.period = Seconds(3);
        h1.inner.timers.progress.max_timeout = Seconds(2);

        client.write("te");
        sleep(Millis(50)).await;
        let res = lazy(|cx| h1.inner.poll_request_payload_inner::<Base>(None, cx)).await;
        assert!(matches!(res, Poll::Ready(Ok(()))));
        assert!(h1.inner.payload.is_some());
        assert_eq!(h1.inner.timers.active, Timer::Payload);
        assert_eq!(h1.inner.io.timer_handle().remains(), Seconds(3));
        assert_eq!(h1.inner.timers.progress.max_timeout, Seconds(2));
    }

    /// A period that expired before pausing is checked on resume.
    #[crate::rt_test]
    async fn test_payload_resume_checks_expired_period() {
        let (client, server) = IoTest::create();
        let (mut h1, mut rx) = exhausted_payload_h1(server, 1000);
        h1.inner.timers.progress.max_timeout = Seconds(4);

        client.write("te");
        sleep(Millis(50)).await;
        let res = lazy(|cx| h1.inner.poll_request_payload_inner::<Base>(None, cx)).await;
        assert!(matches!(
            res,
            Poll::Ready(Err(Either::Left(ProtocolError::SlowPayloadTimeout)))
        ));
        assert_eq!(
            stream_recv(&mut rx).await.unwrap().unwrap(),
            Bytes::from("te")
        );

        // enough data was received, the next period starts
        let (client, server) = IoTest::create();
        let (mut h1, _rx) = exhausted_payload_h1(server, 1);
        h1.inner.timers.progress.max_timeout = Seconds(4);

        client.write("te");
        sleep(Millis(50)).await;
        let res = lazy(|cx| h1.inner.poll_request_payload_inner::<Base>(None, cx)).await;
        assert!(matches!(res, Poll::Ready(Ok(()))));
        assert!(h1.inner.payload.is_some());
        assert_eq!(h1.inner.timers.active, Timer::Payload);
        assert_eq!(h1.inner.io.timer_handle().remains(), Seconds(1));
        assert_eq!(h1.inner.timers.progress.max_timeout, Seconds(3));
    }

    #[crate::rt_test]
    async fn test_header_timeout_does_not_leak_into_payload() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        let config: SharedCfg = SharedCfg::new("SVC")
            .add(
                HttpServiceConfig::new()
                    .set_headers_read_rate(Seconds(10), Seconds(10), 1)
                    .set_payload_read_rate(Seconds(10), Seconds(10), 1)
                    .set_keepalive(KeepAlive::Disabled),
            )
            .into();

        let mut h1 = Dispatcher::new(
            0,
            nio::Io::new(server, config),
            Pipeline::new(
                (),
                fn_service(async |mut req: Request| {
                    while req.payload().recv().await.is_some() {}
                    Ok::<_, io::Error>(Response::Ok().build())
                }),
            ),
            Pipeline::new((), DefaultControlService),
            DispatcherConfig::default(),
        );

        client.write("POST / HTTP/1.1\r\ncontent-length: 4\r\n\r\n");
        sleep(Millis(50)).await;
        h1.inner.io.notify_timeout();
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());

        client.write("body");
        assert!(poll_fn(|cx| Pin::new(&mut h1).poll(cx)).await.is_ok());
        assert!(client.read_any().starts_with(b"HTTP/1.1 200 OK\r\n"));
    }

    #[crate::rt_test]
    async fn test_payload_timeout_does_not_leak_into_keepalive() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        let config: SharedCfg = SharedCfg::new("SVC")
            .add(
                HttpServiceConfig::new()
                    .set_headers_read_rate(Seconds(10), Seconds(10), 1)
                    .set_payload_read_rate(Seconds(10), Seconds(10), 1)
                    .set_keepalive(Seconds(10)),
            )
            .into();

        let mut h1 = Dispatcher::new(
            0,
            nio::Io::new(server, config),
            Pipeline::new(
                (),
                fn_service(async |mut req: Request| {
                    while req.payload().recv().await.is_some() {}
                    Ok::<_, io::Error>(Response::Ok().build())
                }),
            ),
            Pipeline::new((), DefaultControlService),
            DispatcherConfig::default(),
        );

        client.write("POST / HTTP/1.1\r\ncontent-length: 4\r\n\r\n");
        sleep(Millis(50)).await;
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());

        client.write("body");
        sleep(Millis(50)).await;
        h1.inner.io.notify_timeout();
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());

        assert_eq!(h1.inner.timers.active, Timer::KeepAlive);
        assert!(h1.inner.io.is_active());
        sleep(Millis(50)).await;
        assert!(client.read_any().starts_with(b"HTTP/1.1 200 OK\r\n"));

        client.close().await;
        assert!(poll_fn(|cx| Pin::new(&mut h1).poll(cx)).await.is_ok());
    }

    #[crate::rt_test]
    async fn test_partial_next_request_wins_keepalive_timeout() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        let config: SharedCfg = SharedCfg::new("SVC")
            .add(
                HttpServiceConfig::new()
                    .set_headers_read_rate(Seconds(10), Seconds(20), 1)
                    .set_keepalive(Seconds(10)),
            )
            .into();

        let mut h1 = Dispatcher::new(
            0,
            nio::Io::new(server, config),
            Pipeline::new(
                (),
                fn_service(async |_| Ok::<_, io::Error>(Response::Ok().build())),
            ),
            Pipeline::new((), DefaultControlService),
            DispatcherConfig::default(),
        );

        client.write("GET /first HTTP/1.1\r\n\r\n");
        sleep(Millis(50)).await;
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        assert_eq!(h1.inner.timers.active, Timer::KeepAlive);
        sleep(Millis(50)).await;
        assert!(client.read_any().starts_with(b"HTTP/1.1 200 OK\r\n"));

        let partial = "GET /next HTTP/1.1\r\nhost: example.com";
        client.write(partial);
        sleep(Millis(50)).await;
        h1.inner.io.notify_timeout();

        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        assert_eq!(h1.inner.timers.active, Timer::Headers);
        assert_ne!(h1.inner.timers.active, Timer::KeepAlive);
        assert_eq!(h1.inner.timers.progress.consumed, partial.len() as u32);
        assert!(h1.inner.io.is_active());

        client.write("\r\n\r\n");
        sleep(Millis(50)).await;
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        sleep(Millis(50)).await;
        assert!(client.read_any().starts_with(b"HTTP/1.1 200 OK\r\n"));

        client.close().await;
        assert!(poll_fn(|cx| Pin::new(&mut h1).poll(cx)).await.is_ok());
    }

    fn keepalive_h1(server: IoTest) -> Dispatcher<Base, body::Body, io::Error> {
        let config: SharedCfg = SharedCfg::new("SVC")
            .add(
                HttpServiceConfig::new()
                    .set_client_timeout(Seconds::ZERO)
                    .set_keepalive(Seconds(1)),
            )
            .into();

        Dispatcher::new(
            0,
            nio::Io::new(server, config),
            Pipeline::new(
                (),
                fn_service(async |_| Ok::<_, io::Error>(Response::Ok().build())),
            ),
            Pipeline::new((), DefaultControlService),
            DispatcherConfig::default(),
        )
    }

    /// Without request-head timing, waiting for the first request is not
    /// bounded by keep-alive, for an idle or a partially received request.
    #[crate::rt_test]
    async fn test_first_request_unbounded_without_header_timeout() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        let mut h1 = keepalive_h1(server);

        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        assert_eq!(h1.inner.timers.active, Timer::ClientTimeout);
        sleep(Millis(2200)).await;
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        assert!(h1.inner.io.is_active());

        client.write("GET / HTTP/1.1\r\n");
        sleep(Millis(50)).await;
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        assert_eq!(h1.inner.timers.active, Timer::ClientTimeout);
        sleep(Millis(2200)).await;
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        assert!(h1.inner.io.is_active());

        client.write("\r\n");
        sleep(Millis(50)).await;
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        sleep(Millis(50)).await;
        assert!(client.read_any().starts_with(b"HTTP/1.1 200 OK\r\n"));

        client.close().await;
        assert!(poll_fn(|cx| Pin::new(&mut h1).poll(cx)).await.is_ok());
    }

    /// Without request-head timing, a partial next request does not stop the
    /// keep-alive timer.
    #[crate::rt_test]
    async fn test_partial_next_request_keepalive_without_header_timeout() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        let mut h1 = keepalive_h1(server);

        client.write("GET /first HTTP/1.1\r\n\r\nGET /next HTTP/1.1\r\n");
        sleep(Millis(50)).await;
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        sleep(Millis(50)).await;
        assert!(client.read_any().starts_with(b"HTTP/1.1 200 OK\r\n"));
        assert_eq!(h1.inner.timers.active, Timer::KeepAlive);

        client.write("host: example.com");
        sleep(Millis(50)).await;
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        assert_eq!(h1.inner.timers.active, Timer::KeepAlive);

        let res = timeout(Millis(3000), poll_fn(|cx| Pin::new(&mut h1).poll(cx))).await;
        assert!(res.unwrap().is_ok());
        assert!(client.read_any().is_empty());
    }

    fn client_timeout_h1(server: IoTest) -> Dispatcher<Base, body::Body, io::Error> {
        let config: SharedCfg = SharedCfg::new("SVC")
            .add(
                HttpServiceConfig::new()
                    .set_headers_read_rate(Seconds(1), Seconds(5), 1024)
                    .set_keepalive(KeepAlive::Disabled),
            )
            .into();

        Dispatcher::new(
            0,
            nio::Io::new(server, config),
            Pipeline::new(
                (),
                fn_service(async |_| Ok::<_, io::Error>(Response::Ok().build())),
            ),
            Pipeline::new((), DefaultControlService),
            DispatcherConfig::default(),
        )
    }

    /// A new connection that sends nothing is bounded by the client timeout.
    #[crate::rt_test]
    async fn test_client_timeout_without_request() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        let mut h1 = client_timeout_h1(server);

        assert_eq!(h1.inner.timers.active, Timer::ClientTimeout);
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        assert_eq!(h1.inner.timers.active, Timer::ClientTimeout);

        let res = timeout(Millis(3000), poll_fn(|cx| Pin::new(&mut h1).poll(cx))).await;
        assert!(res.unwrap().is_ok());
        assert!(
            client
                .read_any()
                .starts_with(b"HTTP/1.1 408 Request Timeout\r\n")
        );
    }

    /// The headers read rate starts with the first byte of the first request,
    /// with the complete cumulative budget.
    #[crate::rt_test]
    async fn test_headers_rate_starts_with_first_byte() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        let mut h1 = client_timeout_h1(server);

        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        assert_eq!(h1.inner.timers.active, Timer::ClientTimeout);

        let partial = "GET / HTTP/1.1\r\n";
        client.write(partial);
        sleep(Millis(50)).await;
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        assert_eq!(h1.inner.timers.active, Timer::Headers);
        assert_eq!(h1.inner.timers.progress.consumed, partial.len() as u32);
        assert_eq!(h1.inner.timers.progress.max_timeout, Seconds(4));

        client.write("\r\n");
        sleep(Millis(50)).await;
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        sleep(Millis(50)).await;
        assert!(client.read_any().starts_with(b"HTTP/1.1 200 OK\r\n"));
    }

    /// The first byte arriving together with the client timeout wins.
    #[crate::rt_test]
    async fn test_first_byte_wins_client_timeout() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        let mut h1 = client_timeout_h1(server);

        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        client.write("GET / HTTP/1.1\r\n");
        sleep(Millis(50)).await;
        h1.inner.io.notify_timeout();

        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        assert_eq!(h1.inner.timers.active, Timer::Headers);
        assert!(h1.inner.io.is_active());
        assert!(client.read_any().is_empty());
    }

    #[crate::rt_test]
    async fn test_new_connection_without_header_timeout() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        let config: SharedCfg = SharedCfg::new("SVC")
            .add(
                HttpServiceConfig::new()
                    .set_client_timeout(Seconds::ZERO)
                    .set_keepalive(KeepAlive::Disabled),
            )
            .into();

        let mut h1 = Dispatcher::new(
            0,
            nio::Io::new(server, config),
            Pipeline::new(
                (),
                fn_service(async |_| Ok::<_, io::Error>(Response::Ok().build())),
            ),
            Pipeline::new((), DefaultControlService),
            DispatcherConfig::default(),
        );

        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        assert!(h1.inner.io.is_active());

        client.write("GET / HTTP/1.1\r\n\r\n");
        sleep(Millis(50)).await;
        assert!(poll_fn(|cx| Pin::new(&mut h1).poll(cx)).await.is_ok());

        let buf = client.read_any();
        assert!(buf.starts_with(b"HTTP/1.1 200 OK\r\n"));
    }

    /// Create http/1 dispatcher.
    pub(crate) fn h1<F, S, B>(stream: IoTest, s: F) -> Dispatcher<Base, B, S::Error>
    where
        F: IntoService<S, (), Request>,
        S: Service<(), Request> + 'static,
        S::Res: Into<Response<B>>,
        S::Error: ResponseError + 'static,
        B: MessageBody,
    {
        let cfg: SharedCfg = SharedCfg::new("DBG")
            .add(
                HttpServiceConfig::new()
                    .set_keepalive(Seconds(5))
                    .set_client_timeout(Seconds(1)),
            )
            .into();

        Dispatcher::new(
            0,
            nio::Io::new(stream, cfg.clone()),
            Pipeline::new((), s.into_service().map(Into::into)),
            Pipeline::new((), DefaultControlService),
            DispatcherConfig::default(),
        )
    }

    pub(crate) fn spawn_h1<F, S, B>(stream: IoTest, s: F)
    where
        F: IntoService<S, (), Request>,
        S: Service<(), Request> + 'static,
        S::Res: Into<Response<B>>,
        S::Error: ResponseError,
        B: MessageBody + 'static,
    {
        let cfg: SharedCfg = SharedCfg::new("DBG")
            .add(
                HttpServiceConfig::new()
                    .set_keepalive(Seconds(5))
                    .set_client_timeout(Seconds(1)),
            )
            .into();

        crate::rt::spawn(Dispatcher::new(
            0,
            nio::Io::new(stream, cfg),
            Pipeline::new((), s.into_service().map(Into::into)),
            Pipeline::new((), DefaultControlService),
            DispatcherConfig::default(),
        ));
    }

    fn load(decoder: &mut ClientCodec, buf: &mut BytesMut) -> ResponseHead {
        decoder.decode(buf).unwrap().unwrap()
    }

    #[crate::rt_test]
    async fn test_on_request() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        client.write("GET /test HTTP/1.0\r\n\r\n");

        let data = Rc::new(Cell::new(false));
        let data2 = data.clone();
        let config: SharedCfg = SharedCfg::new("DBG")
            .add(
                HttpServiceConfig::new()
                    .set_keepalive(Seconds(5))
                    .set_client_timeout(Seconds(1)),
            )
            .into();

        let mut h1 = Dispatcher::new(
            0,
            nio::Io::new(server, config),
            Pipeline::new((), async |_| Ok::<_, io::Error>(Response::Ok().build())),
            Pipeline::new(
                (),
                fn_service(async move |req: Control<_, _>| {
                    if let Control::Request(_) = req {
                        data2.set(true);
                    }
                    Ok::<_, DispatchError>(req.ack())
                }),
            ),
            DispatcherConfig::default(),
        );
        sleep(Millis(50)).await;
        let _ = lazy(|cx| Pin::new(&mut h1).poll(cx)).await;
        sleep(Millis(50)).await;

        client.local_buffer(|buf| assert_eq!(&buf[..15], b"HTTP/1.0 200 OK"));
        client.close().await;

        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_ready());
        assert!(data.get());
    }

    #[crate::rt_test]
    async fn test_req_parse_err() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        client.write("GET /test HTTP/1\r\n\r\n");

        let mut h1 = h1(server, async |_| Ok::<_, io::Error>(Response::Ok().build()));
        sleep(Millis(50)).await;
        // required because io shutdown is async oper
        let _ = lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_ready();
        sleep(Millis(50)).await;

        assert!(poll_fn(|cx| Pin::new(&mut h1).poll(cx)).await.is_ok());
        assert!(!h1.inner.io.is_active());
        sleep(Millis(50)).await;

        client.local_buffer(|buf| assert_eq!(&buf[..26], b"HTTP/1.1 400 Bad Request\r\n"));

        client.close().await;
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_ready());
        assert!(!h1.inner.io.is_active());
    }

    #[crate::rt_test]
    async fn test_pipeline() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(4096);
        let mut decoder = ClientCodec::new(true, SharedCfg::default().get());
        spawn_h1(server, async |_| Ok::<_, io::Error>(Response::Ok().build()));

        client.write("GET /test1 HTTP/1.1\r\n\r\n");

        let mut buf = BytesMut::from(&client.read().await.unwrap()[..]);
        assert!(load(&mut decoder, &mut buf).status.is_success());
        assert!(!client.is_server_dropped());

        client.write("GET /test2 HTTP/1.1\r\n\r\n");
        client.write("GET /test3 HTTP/1.1\r\n\r\n");

        let mut buf = BytesMut::from(&client.read().await.unwrap()[..]);
        assert!(load(&mut decoder, &mut buf).status.is_success());
        assert!(load(&mut decoder, &mut buf).status.is_success());
        assert!(decoder.decode(&mut buf).unwrap().is_none());
        assert!(!client.is_server_dropped());

        client.close().await;
        assert!(client.is_server_dropped());
    }

    #[crate::rt_test]
    async fn test_pipeline_with_payload() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(4096);
        let mut decoder = ClientCodec::new(true, SharedCfg::default().get());

        spawn_h1(server, async move |mut req: Request| {
            let mut p = req.take_payload();
            while (stream_recv(&mut p).await).is_some() {}
            Ok::<_, io::Error>(Response::Ok().build())
        });

        client.write("GET /test1 HTTP/1.1\r\ncontent-length: 5\r\n\r\n");
        sleep(Millis(50)).await;
        client.write("xxxxx");

        let mut buf = BytesMut::from(&client.read().await.unwrap()[..]);
        assert!(load(&mut decoder, &mut buf).status.is_success());
        assert!(!client.is_server_dropped());

        client.write("GET /test2 HTTP/1.1\r\n\r\n");

        let mut buf = BytesMut::from(&client.read().await.unwrap()[..]);
        assert!(load(&mut decoder, &mut buf).status.is_success());
        assert!(decoder.decode(&mut buf).unwrap().is_none());
        assert!(!client.is_server_dropped());

        client.close().await;
        assert!(client.is_server_dropped());
    }

    #[crate::rt_test]
    async fn test_pipeline_with_delay() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(4096);
        let mut decoder = ClientCodec::new(true, SharedCfg::default().get());
        spawn_h1(server, async |_| {
            sleep(Millis(100)).await;
            Ok::<_, io::Error>(Response::Ok().build())
        });

        client.write("GET /test HTTP/1.1\r\n\r\n");

        let mut buf = BytesMut::from(&client.read().await.unwrap()[..]);
        assert!(load(&mut decoder, &mut buf).status.is_success());
        assert!(!client.is_server_dropped());

        client.write("GET /test HTTP/1.1\r\n\r\n");
        client.write("GET /test HTTP/1.1\r\n\r\n");
        sleep(Millis(50)).await;
        client.write("GET /test HTTP/1.1\r\n\r\n");

        let mut buf = BytesMut::from(&client.read().await.unwrap()[..]);
        assert!(load(&mut decoder, &mut buf).status.is_success());

        let mut buf = BytesMut::from(&client.read().await.unwrap()[..]);
        assert!(load(&mut decoder, &mut buf).status.is_success());
        assert!(decoder.decode(&mut buf).unwrap().is_none());
        assert!(!client.is_server_dropped());

        buf.extend(client.read().await.unwrap());
        assert!(load(&mut decoder, &mut buf).status.is_success());
        assert!(decoder.decode(&mut buf).unwrap().is_none());
        assert!(!client.is_server_dropped());

        client.close().await;
        assert!(client.is_server_dropped());
    }

    #[crate::rt_test]
    /// A peer write-half close still allows all buffered requests to complete
    /// and their responses to be written.
    async fn test_write_disconnected() {
        let num = Arc::new(AtomicUsize::new(0));
        let num2 = num.clone();

        let (client, server) = IoTest::create();
        spawn_h1(server, async move |_| {
            num2.fetch_add(1, Ordering::Relaxed);
            Ok::<_, io::Error>(Response::Ok().build())
        });

        client.remote_buffer_cap(1024);
        client.write("GET /test HTTP/1.1\r\n\r\n");
        client.write("GET /test HTTP/1.1\r\n\r\n");
        client.write("GET /test HTTP/1.1\r\n\r\n");
        client.close().await;
        assert!(client.is_server_dropped());

        let mut decoder = ClientCodec::new(true, SharedCfg::default().get());
        let mut buf = BytesMut::from(&client.read_any()[..]);
        for _ in 0..3 {
            assert!(load(&mut decoder, &mut buf).status.is_success());
        }
        assert!(decoder.decode(&mut buf).unwrap().is_none());
        assert_eq!(num.load(Ordering::Relaxed), 3);
    }

    /// max http message size is 32k (no payload)
    #[crate::rt_test]
    async fn test_read_large_message() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(4096);

        let mut h1 = h1(server, async |_| Ok::<_, io::Error>(Response::Ok().build()));
        // SAFETY: this test does not retain a reference returned by `io.cfg()`.
        unsafe {
            h1.inner.io.set_config(
                SharedCfg::new("TEST")
                    .add(
                        nio::IoConfig::new()
                            .set_read_buf(15 * 1024, 1024, 16)
                            .set_write_buf(15 * 1024),
                    )
                    .add(HttpServiceConfig::new().set_max_buf_size(32 * 1024)),
            );
        }

        let mut decoder = ClientCodec::new(true, SharedCfg::default().get());

        // generate large http message
        let data = rand::rng()
            .sample_iter(&rand::distr::Alphanumeric)
            .take(70_000)
            .map(char::from)
            .collect::<String>();
        client.write("GET /test HTTP/1.1\r\nContent-Length: ");
        client.write(data);
        sleep(Millis(50)).await;

        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        sleep(Millis(50)).await;
        poll_fn(|cx| Pin::new(&mut h1).poll(cx)).await.unwrap();
        assert!(!h1.inner.io.is_active());

        let mut buf = BytesMut::from(&client.read().await.unwrap()[..]);
        assert_eq!(
            load(&mut decoder, &mut buf).status,
            StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE
        );
    }

    #[crate::rt_test]
    async fn test_read_backpressure() {
        let mark = Arc::new(AtomicBool::new(false));
        let mark2 = mark.clone();

        let (client, server) = IoTest::create();
        client.remote_buffer_cap(4096);
        spawn_h1(server, async move |mut req: Request| {
            let m = mark2.clone();

            // read one chunk
            let mut pl = req.take_payload();
            let _ = stream_recv(&mut pl).await.unwrap().unwrap();
            m.store(true, Ordering::Relaxed);
            // sleep
            sleep(Millis(999_999_000)).await;
            Ok::<_, io::Error>(Response::Ok().build())
        });

        client.write("GET /test HTTP/1.1\r\nContent-Length: 1048576\r\n\r\n");
        sleep(Millis(50)).await;

        // buf must be consumed
        assert_eq!(client.remote_buffer(|buf| buf.len()), 0);

        // io should be drained only by no more than MAX_BUFFER_SIZE
        let random_bytes: Vec<u8> = (0..1_048_576).map(|_| rand::random::<u8>()).collect();
        client.write(random_bytes);

        sleep(Millis(50)).await;
        assert!(client.remote_buffer(|buf| buf.len()) > 1_048_576 - BUFFER_SIZE * 3);
        assert!(mark.load(Ordering::Relaxed));
    }

    fn write_timeout_h1(
        server: IoTest,
        cfg: HttpServiceConfig,
        payload: Rc<RefCell<Option<http::Payload>>>,
    ) -> Dispatcher<Base, body::Body, io::Error> {
        let config: SharedCfg = SharedCfg::new("SVC").add(cfg).into();
        Dispatcher::new(
            0,
            nio::Io::new(server, config),
            Pipeline::new(
                (),
                fn_service(move |mut req: Request| {
                    payload.borrow_mut().replace(req.take_payload());
                    async { Ok::<_, io::Error>(Response::Ok().body("x".repeat(128 * 1024))) }
                }),
            ),
            Pipeline::new((), DefaultControlService),
            DispatcherConfig::default(),
        )
    }

    /// A client that stops reading the response is disconnected by the write
    /// timeout.
    #[crate::rt_test]
    async fn test_write_timeout_during_response() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(0);
        let mut h1 = write_timeout_h1(
            server,
            HttpServiceConfig::new().set_write_timeout(Seconds(1)),
            Rc::default(),
        );

        client.write("GET / HTTP/1.1\r\n\r\n");
        sleep(Millis(50)).await;
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        assert!(h1.inner.io.is_wr_backpressure());
        assert_eq!(h1.inner.timers.active, Timer::Write);

        let res = timeout(Millis(3000), poll_fn(|cx| Pin::new(&mut h1).poll(cx))).await;
        assert!(res.unwrap().is_ok());
        assert!(client.is_closed());
    }

    /// The write timeout stops the dispatcher while sending a response with
    /// an unfinished request payload.
    #[crate::rt_test]
    async fn test_write_timeout_with_unfinished_payload() {
        let payload = Rc::new(RefCell::new(None));
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(0);
        let mut h1 = write_timeout_h1(
            server,
            HttpServiceConfig::new().set_write_timeout(Seconds(1)),
            payload.clone(),
        );

        client.write("POST / HTTP/1.1\r\ncontent-length: 4\r\n\r\nb");
        let res = timeout(Millis(3000), poll_fn(|cx| Pin::new(&mut h1).poll(cx))).await;
        assert!(res.unwrap().is_ok());

        let mut payload = payload.borrow_mut().take().unwrap();
        let mut timed_out = false;
        while let Some(item) = payload.recv().await {
            if let Err(PayloadError::Io(err)) = item {
                assert_eq!(err.kind(), io::ErrorKind::TimedOut);
                timed_out = true;
            }
        }
        assert!(timed_out);
    }

    /// A delayed protocol error does not interrupt a response whose head has
    /// been sent.
    #[crate::rt_test]
    async fn test_delayed_protocol_error_finishes_response() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        let mut h1 = write_timeout_h1(server, HttpServiceConfig::new(), Rc::default());
        h1.inner.pending_payload_error = Some(Either::Left(ProtocolError::SlowPayloadTimeout));

        let mut body = ResponseBody::from(body::Body::from("body"));
        let st = lazy(|cx| h1.inner.poll_send_payload(cx, &mut body)).await;
        assert!(matches!(st, Poll::Ready(State::Stop)));
        assert!(matches!(h1.inner.disconnect, Disconnect::Sent));
        assert!(h1.inner.pending_payload_error.is_none());
    }

    /// Without a write timeout, write backpressure is not bounded.
    #[crate::rt_test]
    async fn test_no_write_timeout_during_response() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(0);
        let mut h1 = write_timeout_h1(server, HttpServiceConfig::new(), Rc::default());

        client.write("GET / HTTP/1.1\r\n\r\n");
        sleep(Millis(50)).await;
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        assert_eq!(h1.inner.timers.active, Timer::Stopped);

        let res = timeout(Millis(2500), poll_fn(|cx| Pin::new(&mut h1).poll(cx))).await;
        assert!(res.is_err());
        assert!(!client.is_closed());
    }

    /// The write timer keeps paused payload timing and restores it once
    /// backpressure is disabled.
    #[crate::rt_test]
    async fn test_write_timer_restores_payload_timer() {
        let payload = Rc::new(RefCell::new(None));
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(0);
        let mut h1 = write_timeout_h1(
            server,
            HttpServiceConfig::new()
                .set_write_timeout(Seconds(5))
                .set_payload_read_rate(Seconds(10), Seconds(20), 1),
            payload.clone(),
        );

        client.write("POST / HTTP/1.1\r\ncontent-length: 4\r\n\r\nb");
        sleep(Millis(50)).await;
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        assert_eq!(h1.inner.timers.active, Timer::WriteWithPayload);
        assert_eq!(h1.inner.timers.progress.period, Seconds(10));
        assert_eq!(h1.inner.timers.progress.max_timeout, Seconds(10));

        client.remote_buffer_cap(256 * 1024);
        sleep(Millis(50)).await;
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        assert_eq!(h1.inner.timers.active, Timer::Payload);

        payload.borrow_mut().take();
        client.close().await;
        assert!(poll_fn(|cx| Pin::new(&mut h1).poll(cx)).await.is_ok());
    }

    #[crate::rt_test]
    async fn test_payload_timer_pauses_for_write_backpressure() {
        let payload = Rc::new(RefCell::new(None));
        let payload2 = payload.clone();
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(0);

        let config: SharedCfg = SharedCfg::new("SVC")
            .add(HttpServiceConfig::new().set_payload_read_rate(Seconds(10), Seconds(20), 1))
            .into();

        let mut h1 = Dispatcher::new(
            0,
            nio::Io::new(server, config),
            Pipeline::new(
                (),
                fn_service(move |mut req: Request| {
                    payload2.borrow_mut().replace(req.take_payload());
                    async { Ok::<_, io::Error>(Response::Ok().body("x".repeat(128 * 1024))) }
                }),
            ),
            Pipeline::new((), DefaultControlService),
            DispatcherConfig::default(),
        );

        client.write("POST / HTTP/1.1\r\ncontent-length: 4\r\n\r\nb");
        sleep(Millis(50)).await;
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        assert_eq!(h1.inner.timers.active, Timer::Payload);
        assert!(h1.inner.io.is_wr_backpressure());

        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        assert_eq!(h1.inner.timers.active, Timer::PayloadPaused);
        assert_ne!(h1.inner.timers.active, Timer::Payload);
        assert!(!h1.inner.io.timer_handle().is_set());

        client.remote_buffer_cap(256 * 1024);
        sleep(Millis(50)).await;
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        assert_eq!(h1.inner.timers.active, Timer::Payload);
        assert_ne!(h1.inner.timers.active, Timer::PayloadPaused);

        client.write("ody");
        sleep(Millis(50)).await;
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());

        payload.borrow_mut().take();
        client.close().await;
        assert!(poll_fn(|cx| Pin::new(&mut h1).poll(cx)).await.is_ok());
    }

    #[crate::rt_test]
    #[allow(clippy::items_after_statements)]
    async fn test_write_backpressure() {
        let num = Arc::new(AtomicUsize::new(0));
        let num2 = num.clone();

        struct Stream(Arc<AtomicUsize>);

        impl body::MessageBody for Stream {
            fn size(&self) -> body::BodySize {
                body::BodySize::Stream
            }
            fn poll_next_chunk(
                &mut self,
                _: &mut Context<'_>,
            ) -> Poll<Option<Result<Bytes, Rc<dyn error::Error>>>> {
                let data = rand::rng()
                    .sample_iter(&rand::distr::Alphanumeric)
                    .take(65_536)
                    .map(char::from)
                    .collect::<String>();
                self.0.fetch_add(data.len(), Ordering::Relaxed);

                Poll::Ready(Some(Ok(Bytes::from(data))))
            }
        }

        let (client, server) = IoTest::create();
        let mut h1 = h1(server, async move |_| {
            let n = num2.clone();
            Ok::<_, io::Error>(Response::Ok().message_body(Stream(n.clone())))
        });
        let state = h1.inner.io.get_ref();

        // do not allow to write to socket
        client.remote_buffer_cap(0);
        client.write("GET /test HTTP/1.1\r\n\r\n");
        sleep(Millis(50)).await;
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());

        // buf must be consumed
        assert_eq!(client.remote_buffer(|buf| buf.len()), 0);

        // amount of generated data
        assert_eq!(num.load(Ordering::Relaxed), 65_536);

        // response message + chunking encoding
        assert_eq!(state.with_write_src(|buf| buf.len()).unwrap(), 65629);

        client.remote_buffer_cap(65536);
        sleep(Millis(50)).await;
        assert_eq!(state.with_write_src(|buf| { buf.len() }).unwrap(), 93);

        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        assert_eq!(num.load(Ordering::Relaxed), 65_536 * 2);
    }

    #[crate::rt_test]
    async fn test_disconnect_during_response_body_pending() {
        struct Stream(bool);

        impl body::MessageBody for Stream {
            fn size(&self) -> body::BodySize {
                body::BodySize::Sized(2048)
            }
            fn poll_next_chunk(
                &mut self,
                _: &mut Context<'_>,
            ) -> Poll<Option<Result<Bytes, Rc<dyn error::Error>>>> {
                if self.0 {
                    Poll::Pending
                } else {
                    self.0 = true;
                    let data = rand::rng()
                        .sample_iter(&rand::distr::Alphanumeric)
                        .take(1024)
                        .map(char::from)
                        .collect::<String>();
                    Poll::Ready(Some(Ok(Bytes::from(data))))
                }
            }
        }

        let (client, server) = IoTest::create();
        client.remote_buffer_cap(4096);
        let mut h1 = h1(server, async |_| {
            Ok::<_, io::Error>(Response::Ok().message_body(Stream(false)))
        });

        client.write("GET /test HTTP/1.1\r\n\r\n");
        sleep(Millis(50)).await;
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());

        // http message must be consumed
        assert_eq!(client.remote_buffer(|buf| buf.len()), 0);

        let mut decoder = ClientCodec::new(true, SharedCfg::default().get());
        let mut buf = BytesMut::from(&client.read().await.unwrap()[..]);
        assert!(load(&mut decoder, &mut buf).status.is_success());
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());

        client.close().await;
        sleep(Millis(50)).await;
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
    }

    #[crate::rt_test]
    async fn test_partial_request_headers_clean_disconnect() {
        let requests = Arc::new(AtomicUsize::new(0));
        let requests2 = requests.clone();
        let disconnects = Arc::new(AtomicUsize::new(0));
        let disconnects2 = disconnects.clone();

        let (client, server) = IoTest::create();
        let config: SharedCfg = SharedCfg::new("SVC")
            .add(HttpServiceConfig::new().set_client_timeout(Seconds(10)))
            .into();

        let mut h1 = Dispatcher::new(
            0,
            nio::Io::new(server, config),
            Pipeline::new(
                (),
                fn_service(async move |_| {
                    requests2.fetch_add(1, Ordering::Relaxed);
                    Ok::<_, io::Error>(Response::Ok().build())
                }),
            ),
            Pipeline::new(
                (),
                fn_service(async move |msg: Control<_, _>| {
                    if let Control::Disconnect(Reason::PeerGone(err)) = &msg {
                        assert!(err.get_ref().is_none());
                        disconnects2.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok::<_, DispatchError>(msg.ack())
                }),
            ),
            DispatcherConfig::default(),
        );

        client.write("GET /test HTTP/1.1\r\nHost: example.com");
        sleep(Millis(50)).await;
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());

        client.close().await;
        assert!(poll_fn(|cx| Pin::new(&mut h1).poll(cx)).await.is_ok());
        assert_eq!(requests.load(Ordering::Relaxed), 0);
        assert_eq!(disconnects.load(Ordering::Relaxed), 1);
        assert!(client.read_any().is_empty());
    }

    #[crate::rt_test]
    async fn test_partial_request_headers_read_error() {
        let requests = Arc::new(AtomicUsize::new(0));
        let requests2 = requests.clone();
        let disconnects = Arc::new(AtomicUsize::new(0));
        let disconnects2 = disconnects.clone();

        let (client, server) = IoTest::create();
        let config: SharedCfg = SharedCfg::new("SVC")
            .add(HttpServiceConfig::new().set_client_timeout(Seconds(10)))
            .into();

        let mut h1 = Dispatcher::new(
            0,
            nio::Io::new(server, config),
            Pipeline::new(
                (),
                fn_service(async move |_| {
                    requests2.fetch_add(1, Ordering::Relaxed);
                    Ok::<_, io::Error>(Response::Ok().build())
                }),
            ),
            Pipeline::new(
                (),
                fn_service(async move |msg: Control<_, _>| {
                    if let Control::Disconnect(Reason::PeerGone(err)) = &msg {
                        assert_eq!(
                            err.get_ref().map(io::Error::kind),
                            Some(io::ErrorKind::ConnectionReset)
                        );
                        disconnects2.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok::<_, DispatchError>(msg.ack())
                }),
            ),
            DispatcherConfig::default(),
        );

        client.write("GET /test HTTP/1.1\r\nHost: example.com");
        sleep(Millis(50)).await;
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());

        client.read_error(io::Error::new(
            io::ErrorKind::ConnectionReset,
            "connection reset",
        ));
        assert!(poll_fn(|cx| Pin::new(&mut h1).poll(cx)).await.is_ok());
        assert_eq!(requests.load(Ordering::Relaxed), 0);
        assert_eq!(disconnects.load(Ordering::Relaxed), 1);
        assert!(client.read_any().is_empty());
    }

    #[crate::rt_test]
    async fn test_header_progress_available_when_timer_fires() {
        let (client, server) = IoTest::create();
        let config: SharedCfg = SharedCfg::new("SVC")
            .add(HttpServiceConfig::new().set_headers_read_rate(Seconds(1), Seconds(2), 4))
            .into();

        let mut h1 = Dispatcher::new(
            0,
            nio::Io::new(server, config),
            Pipeline::new(
                (),
                fn_service(async |_| Ok::<_, io::Error>(Response::Ok().build())),
            ),
            Pipeline::new((), DefaultControlService),
            DispatcherConfig::default(),
        );

        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        client.write("GET /");
        sleep(Millis(1100)).await;

        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        sleep(Millis(50)).await;
        assert!(client.read_any().is_empty());

        client.close().await;
        assert!(poll_fn(|cx| Pin::new(&mut h1).poll(cx)).await.is_ok());
    }

    #[crate::rt_test]
    async fn test_payload_progress_available_when_timer_fires() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(4096);

        let config: SharedCfg = SharedCfg::new("SVC")
            .add(HttpServiceConfig::new().set_payload_read_rate(Seconds(1), Seconds(2), 2))
            .into();

        let mut h1 = Dispatcher::new(
            0,
            nio::Io::new(server, config),
            Pipeline::new(
                (),
                fn_service(async |mut req: Request| {
                    while req.payload().recv().await.is_some() {}
                    Ok::<_, io::Error>(Response::Ok().build())
                }),
            ),
            Pipeline::new((), DefaultControlService),
            DispatcherConfig::default(),
        );

        client.write("POST / HTTP/1.1\r\ntransfer-encoding: chunked\r\n\r\n");
        sleep(Millis(50)).await;
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());

        // Chunk framing is consumed without producing a payload item.
        client.write("4\r\n");
        sleep(Millis(1100)).await;
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        sleep(Millis(50)).await;
        assert!(client.read_any().is_empty());

        client.close().await;
        assert!(poll_fn(|cx| Pin::new(&mut h1).poll(cx)).await.is_ok());
    }

    #[crate::rt_test]
    async fn test_payload_timer_waits_for_expect_continue() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(4096);

        let config: SharedCfg = SharedCfg::new("SVC")
            .add(
                HttpServiceConfig::new()
                    .set_payload_read_rate(Seconds(1), Seconds(2), 1)
                    .set_keepalive(KeepAlive::Disabled),
            )
            .into();

        let h1 = Dispatcher::new(
            0,
            nio::Io::new(server, config),
            Pipeline::new(
                (),
                fn_service(async |mut req: Request| {
                    let mut body = BytesMut::new();
                    while let Some(chunk) = req.payload().recv().await {
                        body.extend_from_slice(&chunk.unwrap());
                    }
                    assert_eq!(&body[..], b"test");
                    Ok::<_, io::Error>(Response::Ok().build())
                }),
            ),
            Pipeline::new(
                (),
                fn_service(async |req: Control<Base, io::Error>| {
                    if let Control::Expect(exc) = req {
                        // slower than the payload read-rate limits
                        sleep(Millis(3100)).await;
                        Ok::<_, DispatchError>(exc.ack())
                    } else {
                        Ok(req.ack())
                    }
                }),
            ),
            DispatcherConfig::default(),
        );
        crate::rt::spawn(h1);

        client.write("POST / HTTP/1.1\r\ncontent-length: 4\r\nexpect: 100-continue\r\n\r\n");
        sleep(Millis(3300)).await;
        let buf = client.read_any();
        assert_eq!(&buf[..], b"HTTP/1.1 100 Continue\r\n\r\n");

        client.write("test");
        sleep(Millis(100)).await;
        let buf = client.read_any();
        assert!(buf.starts_with(b"HTTP/1.1 200 OK\r\n"), "{buf:?}");
    }

    /// An expectation answered with a final response starts payload timing
    /// once the dispatcher reads the rest of the payload.
    #[crate::rt_test]
    async fn test_payload_timer_starts_after_expect_response() {
        let stash = Rc::new(RefCell::new(None));
        let stash2 = stash.clone();
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        let config: SharedCfg = SharedCfg::new("SVC")
            .add(HttpServiceConfig::new().set_payload_read_rate(Seconds(1), Seconds(2), 1))
            .into();

        let mut h1 = Dispatcher::new(
            0,
            nio::Io::new(server, config),
            Pipeline::new(
                (),
                fn_service(async |_| Ok::<_, io::Error>(Response::Ok().build())),
            ),
            Pipeline::new(
                (),
                fn_service(move |req: Control<Base, io::Error>| {
                    let stash = stash2.clone();
                    async move {
                        if let Control::Request(mut req) = req {
                            // keep the payload stream alive
                            stash.borrow_mut().replace(req.get_mut().take_payload());
                            Ok::<_, DispatchError>(req.fail_with(Response::Forbidden().build()))
                        } else {
                            Ok(req.ack())
                        }
                    }
                }),
            ),
            DispatcherConfig::default(),
        );

        client.write("POST / HTTP/1.1\r\ncontent-length: 4\r\nexpect: 100-continue\r\n\r\n");
        sleep(Millis(50)).await;
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        sleep(Millis(50)).await;
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        assert!(client.read_any().starts_with(b"HTTP/1.1 403 Forbidden\r\n"));
        assert!(matches!(h1.st, State::ReadPayload));
        assert_eq!(h1.inner.timers.active, Timer::Payload);

        let res = timeout(Millis(3500), poll_fn(|cx| Pin::new(&mut h1).poll(cx))).await;
        assert!(res.unwrap().is_ok());
        assert!(stash.borrow().is_some());
    }

    #[crate::rt_test]
    async fn test_payload_peer_gone_while_control_pending() {
        let requests = Arc::new(AtomicUsize::new(0));
        let requests2 = requests.clone();
        let disconnects = Arc::new(AtomicUsize::new(0));
        let disconnects2 = disconnects.clone();
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(4096);

        let mut h1 = Dispatcher::new(
            0,
            nio::Io::new(server, SharedCfg::default()),
            Pipeline::new(
                (),
                fn_service(async move |_| {
                    requests2.fetch_add(1, Ordering::Relaxed);
                    Ok::<_, io::Error>(Response::Ok().build())
                }),
            ),
            Pipeline::new(
                (),
                fn_service(async move |msg: Control<_, _>| {
                    match &msg {
                        Control::Request(_) => sleep(Millis(100)).await,
                        Control::Disconnect(Reason::PeerGone(_)) => {
                            disconnects2.fetch_add(1, Ordering::Relaxed);
                        }
                        _ => {}
                    }
                    Ok::<_, DispatchError>(msg.ack())
                }),
            ),
            DispatcherConfig::default(),
        );

        client.write("POST / HTTP/1.1\r\ncontent-length: 10\r\n\r\nbody");
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());

        client.close().await;
        assert!(poll_fn(|cx| Pin::new(&mut h1).poll(cx)).await.is_ok());
        assert_eq!(requests.load(Ordering::Relaxed), 0);
        assert_eq!(disconnects.load(Ordering::Relaxed), 1);
    }

    #[crate::rt_test]
    async fn test_payload_decode_error_while_control_pending() {
        let requests = Arc::new(AtomicUsize::new(0));
        let requests2 = requests.clone();
        let errors = Arc::new(AtomicUsize::new(0));
        let errors2 = errors.clone();
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(4096);

        let mut h1 = Dispatcher::new(
            0,
            nio::Io::new(server, SharedCfg::default()),
            Pipeline::new(
                (),
                fn_service(async move |_| {
                    requests2.fetch_add(1, Ordering::Relaxed);
                    Ok::<_, io::Error>(Response::Ok().build())
                }),
            ),
            Pipeline::new(
                (),
                fn_service(async move |msg: Control<_, _>| {
                    match &msg {
                        Control::Request(_) => sleep(Millis(100)).await,
                        Control::Disconnect(Reason::ProtocolError(err))
                            if matches!(err.get_ref(), ProtocolError::Decode(_)) =>
                        {
                            errors2.fetch_add(1, Ordering::Relaxed);
                        }
                        _ => {}
                    }
                    Ok::<_, DispatchError>(msg.ack())
                }),
            ),
            DispatcherConfig::default(),
        );

        client.write("POST / HTTP/1.1\r\ntransfer-encoding: chunked\r\n\r\ninvalid\r\n");
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        assert!(poll_fn(|cx| Pin::new(&mut h1).poll(cx)).await.is_ok());
        assert_eq!(requests.load(Ordering::Relaxed), 0);
        assert_eq!(errors.load(Ordering::Relaxed), 1);
    }

    #[crate::rt_test]
    async fn test_disconnect_notification_is_not_repeated() {
        let payload = Rc::new(RefCell::new(None));
        let payload2 = payload.clone();
        let disconnects = Arc::new(AtomicUsize::new(0));
        let disconnects2 = disconnects.clone();
        let peer_gone = Arc::new(AtomicUsize::new(0));
        let peer_gone2 = peer_gone.clone();
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(4096);

        let mut h1 = Dispatcher::new(
            0,
            nio::Io::new(server, SharedCfg::default()),
            Pipeline::new(
                (),
                fn_service(move |mut req: Request| {
                    payload2.borrow_mut().replace(req.take_payload());
                    async { Err::<Response<()>, _>(io::Error::other("service error")) }
                }),
            ),
            Pipeline::new(
                (),
                fn_service(async move |msg: Control<_, io::Error>| {
                    let wait = match &msg {
                        Control::Disconnect(Reason::Error(_)) => {
                            disconnects2.fetch_add(1, Ordering::Relaxed);
                            true
                        }
                        Control::Disconnect(Reason::PeerGone(_)) => {
                            disconnects2.fetch_add(1, Ordering::Relaxed);
                            peer_gone2.fetch_add(1, Ordering::Relaxed);
                            false
                        }
                        _ => false,
                    };
                    if wait {
                        sleep(Millis(100)).await;
                    }
                    Ok::<_, DispatchError>(msg.ack())
                }),
            ),
            DispatcherConfig::default(),
        );

        client.write("POST / HTTP/1.1\r\ncontent-length: 10\r\n\r\nbody");
        sleep(Millis(50)).await;
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());

        client.close().await;
        assert!(poll_fn(|cx| Pin::new(&mut h1).poll(cx)).await.is_ok());
        assert_eq!(disconnects.load(Ordering::Relaxed), 1);
        assert_eq!(peer_gone.load(Ordering::Relaxed), 0);
    }

    #[crate::rt_test]
    async fn test_payload_peer_gone_reports_incomplete() {
        let mark = Arc::new(AtomicUsize::new(0));
        let mark2 = mark.clone();
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(4096);

        let mut h1 = h1(server, move |mut req: Request| {
            let mark = mark2.clone();
            async move {
                while let Some(item) = req.payload().recv().await {
                    if matches!(item, Err(PayloadError::Incomplete(None))) {
                        mark.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Ok::<_, io::Error>(Response::Ok().build())
            }
        });

        client.write("POST / HTTP/1.1\r\ncontent-length: 10\r\n\r\nbody");
        sleep(Millis(50)).await;
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());

        client.close().await;
        assert!(poll_fn(|cx| Pin::new(&mut h1).poll(cx)).await.is_ok());
        timeout(Millis(1000), async {
            while mark.load(Ordering::Relaxed) == 0 {
                sleep(Millis(10)).await;
            }
        })
        .await
        .expect("payload consumer did not receive incomplete error");
        assert_eq!(mark.load(Ordering::Relaxed), 1);
    }

    #[crate::rt_test]
    async fn test_payload_read_error_reports_incomplete() {
        let mark = Arc::new(AtomicUsize::new(0));
        let mark2 = mark.clone();
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(4096);

        let mut h1 = h1(server, move |mut req: Request| {
            let mark = mark2.clone();
            async move {
                while let Some(item) = req.payload().recv().await {
                    if let Err(PayloadError::Incomplete(Some(err))) = item
                        && err.kind() == io::ErrorKind::ConnectionReset
                    {
                        mark.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Ok::<_, io::Error>(Response::Ok().build())
            }
        });

        client.write("POST / HTTP/1.1\r\ncontent-length: 10\r\n\r\nbody");
        sleep(Millis(50)).await;
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());

        client.read_error(io::Error::new(
            io::ErrorKind::ConnectionReset,
            "connection reset",
        ));
        assert!(poll_fn(|cx| Pin::new(&mut h1).poll(cx)).await.is_ok());
        sleep(Millis(50)).await;
        assert_eq!(mark.load(Ordering::Relaxed), 1);
    }

    #[crate::rt_test]
    async fn test_payload_decode_error_is_preserved() {
        let mark = Arc::new(AtomicUsize::new(0));
        let mark2 = mark.clone();
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(4096);

        let mut h1 = h1(server, move |mut req: Request| {
            let mark = mark2.clone();
            async move {
                while let Some(item) = req.payload().recv().await {
                    if matches!(item, Err(PayloadError::Decode(_))) {
                        mark.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Ok::<_, io::Error>(Response::Ok().build())
            }
        });

        client.write("POST / HTTP/1.1\r\ntransfer-encoding: chunked\r\n\r\ninvalid\r\n");
        assert!(poll_fn(|cx| Pin::new(&mut h1).poll(cx)).await.is_ok());
        sleep(Millis(50)).await;
        assert_eq!(mark.load(Ordering::Relaxed), 1);
    }

    #[crate::rt_test]
    async fn test_service_error() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(4096);
        client.write("GET /test HTTP/1.1\r\ncontent-length:512\r\n\r\n");

        let mut h1 = h1(server, |_| {
            Box::pin(async { Err::<Response<()>, _>(io::Error::other("error")) })
        });
        // required because io shutdown is async oper
        assert!(poll_fn(|cx| Pin::new(&mut h1).poll(cx)).await.is_ok());

        assert!(!h1.inner.io.is_active());
        let buf = client.local_buffer(BytesMut::take);
        assert_eq!(&buf[..28], b"HTTP/1.1 500 Internal Server");
        assert_eq!(&buf[buf.len() - 5..], b"error");
    }

    #[crate::rt_test]
    async fn test_payload_timeout() {
        let mark = Arc::new(AtomicUsize::new(0));
        let mark2 = mark.clone();
        let err_mark = Arc::new(AtomicUsize::new(0));
        let err_mark2 = err_mark.clone();

        let (client, server) = IoTest::create();
        client.remote_buffer_cap(4096);

        let svc = move |mut req: Request| {
            let m = mark2.clone();
            async move {
                // read one chunk
                let mut pl = req.take_payload();
                while let Some(item) = stream_recv(&mut pl).await {
                    let size = m.load(Ordering::Relaxed);
                    if let Ok(buf) = item {
                        m.store(size + buf.len(), Ordering::Relaxed);
                    } else {
                        return Ok::<_, io::Error>(Response::Ok().build());
                    }
                }
                Ok::<_, io::Error>(Response::Ok().build())
            }
        };

        let config: SharedCfg = SharedCfg::new("SVC")
            .add(
                HttpServiceConfig::new()
                    .set_keepalive(Seconds(5))
                    .set_client_timeout(Seconds(1))
                    .set_payload_read_rate(Seconds(1), Seconds(2), 512),
            )
            .into();

        let disp = Dispatcher::new(
            0,
            nio::Io::new(server, config),
            Pipeline::new((), fn_service(svc)),
            Pipeline::new(
                (),
                fn_service(async move |msg: Control<_, _>| {
                    if let Control::Disconnect(Reason::ProtocolError(ref err)) = msg
                        && matches!(err.get_ref(), ProtocolError::SlowPayloadTimeout)
                    {
                        err_mark2.store(err_mark2.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
                    }
                    Ok::<_, DispatchError>(msg.ack())
                }),
            ),
            DispatcherConfig::default(),
        );
        crate::rt::spawn(disp);

        client.write("GET /test HTTP/1.1\r\nContent-Length: 1048576\r\n\r\n");
        sleep(Millis(50)).await;

        // send partial data to server
        for _ in 1..8 {
            let random_bytes: Vec<u8> = (0..256).map(|_| rand::random::<u8>()).collect();
            client.write(random_bytes);
            sleep(Millis(750)).await;
        }
        // The first interval exceeds the configured rate and earns one
        // extension; the two-second maximum then terminates the payload.
        assert_eq!(mark.load(Ordering::Relaxed), 1536);
        assert_eq!(err_mark.load(Ordering::Relaxed), 1);
    }

    #[crate::rt_test]
    async fn test_unconsumed_payload() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(4096);
        client.write("GET /test HTTP/1.1\r\ncontent-length:512\r\n\r\n");

        let mut h1 = h1(server, async move |_| {
            Ok::<_, io::Error>(Response::Ok().body("TEST"))
        });
        // required because io shutdown is async oper
        assert!(poll_fn(|cx| Pin::new(&mut h1).poll(cx)).await.is_ok());

        assert!(!h1.inner.io.is_active());
        let buf = client.local_buffer(BytesMut::take);
        assert_eq!(
            &buf[..55],
            b"HTTP/1.1 200 OK\r\ncontent-length: 4\r\nconnection: close\r\n"
        );
    }
}
