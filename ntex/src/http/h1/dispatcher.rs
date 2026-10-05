//! HTTP/1 protocol dispatcher
use std::{future, io, mem, pin::Pin, rc::Rc, task::Context, task::Poll, task::ready};

use crate::io::{Decoded, Filter, Io, IoStatusUpdate, RecvError};
use crate::service::pipeline::{Pipeline, PipelineCall};
use crate::{channel::bstream, util::clone_io_error};

use crate::http::body::{Body, MessageBody, ResponseBody};
use crate::http::error::{DispatchError, PayloadError, ResponseError};
use crate::http::message::CurrentIo;
use crate::http::{
    self, StatusCode, config::DispatcherConfig, request::Request, response::Response,
};

use super::control::{Control, ControlAck, ControlResult, ServiceDisconnectReason};
use super::decoder::{PayloadDecoder, PayloadItem, PayloadType};
use super::payload::{Payload, PayloadSender};
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

/// Request payload failure
#[derive(Debug)]
enum PayloadFailure {
    /// Protocol error or payload read timeout
    Protocol(ProtocolError),
    /// Peer is gone or write backpressure timeout
    PeerGone(Option<io::Error>),
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
    control: Option<Pipeline<Control<F, Err>, ControlAck<F>, DispatchError>>,
    payload: Option<(PayloadDecoder, PayloadSender)>,
    pending_payload_error: Option<PayloadFailure>,
    /// The response head is sent and its body is not complete
    response_started: bool,
}

impl<F, B, Err> Dispatcher<F, B, Err>
where
    F: Filter,
    B: MessageBody,
    Err: ResponseError + 'static,
{
    /// Construct new `Dispatcher` instance with outgoing messages stream.
    ///
    /// Without a control service the default action is applied to every
    /// control message.
    pub(in crate::http) fn new(
        id: usize,
        io: Io<F>,
        service: Pipeline<Request, Response<B>, Err>,
        control: Option<Pipeline<Control<F, Err>, ControlAck<F>, DispatchError>>,
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
                response_started: false,
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
                                && let Poll::Ready(Err(err)) = inner.poll_request_payload_inner(cx)
                            {
                                inner.pending_payload_error = Some(err);
                            }
                            return Poll::Pending;
                        }
                    };

                    match result {
                        Ok(ControlAck { result }) => inner.control_result(result),
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
                        // a partially decoded payload keeps reading the payload
                        ready!(result).unwrap_or_else(|| {
                            if inner.payload.is_some() {
                                State::ReadPayload
                            } else {
                                inner.next_request()
                            }
                        })
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

impl<F, B, Err> Drop for DispatcherInner<F, B, Err> {
    fn drop(&mut self) {
        // a detached service call must not wait for the rest of the payload
        if let Some((_, sender)) = self.payload.take() {
            sender.set_error(PayloadError::Incomplete(None));
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
        loop {
            // stop dispatcher
            if self.config.is_shutdown() {
                log::trace!("{}: Service is shutting down", self.io.tag());
                return Poll::Ready(self.ctl_svc_disconnect(ServiceDisconnectReason::Shutdown));
            }

            log::trace!("{}: Trying to read http message", self.io.tag());
            self.release_write_timer();
            if !self.io.is_read_filter_paused() {
                self.timers.resume_read(&self.io);
            }

            let (result, timeout) = match self.io.poll_recv_decode(&self.codec, cx) {
                // a request head received together with a read timer wins over
                // its expiry, the buffered input is decoded first
                Err(RecvError::Timeout)
                    if !self.timers.active.is_write() && self.timers.active != Timer::Idle =>
                {
                    let result = self.io.decode_item(&self.codec);
                    (result.map_err(RecvError::Decoder), true)
                }
                result => (result, false),
            };

            let result = match result {
                Ok(decoded) => {
                    self.timers.headers_decoded(
                        (decoded.consumed + decoded.remains) as u32,
                        decoded.remains as u32,
                    );
                    if timeout && decoded.item.is_none() {
                        match self.headers_timeout(&decoded) {
                            Ok(()) => continue,
                            Err(st) => return Poll::Ready(st),
                        }
                    }
                    if let Some(st) = self.update_hdrs_timer(&decoded) {
                        return Poll::Ready(st);
                    }
                    if let Some(item) = decoded.item {
                        Ok(item)
                    } else {
                        // the filter chain pauses reading, the peer is not
                        // charged for it
                        if self.io.is_read_filter_paused() {
                            self.timers.suspend_read(&self.io);
                        }
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
                            let (ps, pl) = Payload::create();
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
                        continue;
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
                Err(RecvError::Timeout) => {
                    if self.timers.active.is_write() {
                        if let Err(err) = self.write_timer_expired() {
                            self.ctl_peer_gone(Some(err))
                        } else {
                            continue;
                        }
                    } else {
                        // expiry of the keep-alive timer left armed for the
                        // previous request
                        continue;
                    }
                }
            };

            return Poll::Ready(st);
        }
    }

    fn send_response(
        &mut self,
        mut msg: Response<()>,
        mut body: ResponseBody<B>,
    ) -> State<F, B, Err> {
        // an interim response cannot complete the request, the next response would
        // be taken as its final response, see RFC 9110 section 15.2
        let status = msg.status();
        if status.is_informational() && status != StatusCode::SWITCHING_PROTOCOLS {
            log::error!(
                "{}: Informational response {status} is not supported, sending 500",
                self.io.tag()
            );
            msg = Response::new(StatusCode::INTERNAL_SERVER_ERROR).drop_body();
            body = ResponseBody::Other(Body::Empty);
        }

        let size = body.size();
        log::trace!(
            "{}: Sending response: {:?} body: {:?}",
            self.io.tag(),
            msg,
            size
        );
        // close connection if payload stream is dropped and not consumed, or
        // the connection is going to be closed after the response
        if matches!(self.disconnect, Disconnect::Pending(_))
            || self
                .payload
                .as_ref()
                .is_some_and(|(_, snd)| snd.is_closed())
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
                .encode(Message::Item((msg, size)), &self.codec)
                .inspect_err(|_| {
                    if let Some(ref mut payload) = self.payload {
                        payload.1.set_error(PayloadError::Incomplete(None));
                    }
                });

            match result {
                // a response without body bytes, for example to a HEAD request,
                // is complete, the body is not polled
                Ok(()) if self.codec.is_body_complete() => self.response_done(),
                Ok(()) => {
                    self.response_started = true;
                    State::SendPayload { body }
                }
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
            && let Poll::Ready(Err(err)) = self.poll_request_payload_inner(cx)
        {
            Some(err)
        } else {
            None
        };
        match payload_err {
            // peer is gone or write timeout, the response cannot be completed
            Some(PayloadFailure::PeerGone(err)) => return Poll::Ready(self.ctl_peer_gone(err)),
            // the response head is sent, an error response is not possible,
            // finish the response and then stop
            Some(PayloadFailure::Protocol(err)) => {
                log::trace!("{}: Request payload error: {:?}", self.io.tag(), err);
                self.disconnect = Disconnect::Sent;
            }
            None => (),
        }
        loop {
            // encoding is a no-op once the transport is closing, the body
            // would be polled without write backpressure
            if !self.io.is_active() {
                return Poll::Ready(self.ctl_peer_gone(None));
            }
            if self.io.is_wr_backpressure() {
                if let Err(err) = ready!(self.poll_flush_timed(cx)) {
                    return Poll::Ready(self.ctl_peer_gone(Some(err)));
                }
            } else {
                // encoding enables write backpressure once the output exceeds
                // the high watermark, otherwise flushing would not wait
                self.release_write_timer();
            }
            let Poll::Ready(item) = body.poll_next_chunk(cx) else {
                // the client half-closed the connection and has no pipelined
                // requests, an idle body would hold the connection until its
                // next chunk
                if !self.codec.cfg.half_close
                    && self.io.is_read_eof()
                    && self.io.read_dst_size() == 0
                {
                    return Poll::Ready(self.ctl_peer_gone(None));
                }
                // a peer disconnect must be observed while the body is pending
                self.io.register_dispatch(cx);
                return Poll::Pending;
            };

            let st = match item {
                // the declared length is sent or the response has no body,
                // the body is not polled further
                Some(Ok(_)) if self.codec.is_body_complete() => {
                    log::trace!("{}: Response body exceeds its length", self.io.tag());
                    self.response_done()
                }
                Some(Ok(item)) => {
                    log::trace!("{}: Got response chunk: {:?}", self.io.tag(), item.len());
                    match self.io.encode(Message::Chunk(Some(item)), &self.codec) {
                        // the declared length is sent, the body is not polled
                        // for its end, under write backpressure the output is
                        // flushed first
                        Ok(())
                            if self.codec.is_body_complete() && !self.io.is_wr_backpressure() =>
                        {
                            self.response_done()
                        }
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
                    // the end of a body with a declared length encodes nothing
                    if self.codec.is_body_complete() {
                        self.response_done()
                    } else if let Err(err) = self.io.encode(Message::Chunk(None), &self.codec) {
                        self.ctl_proto_err(err.into())
                    } else {
                        self.response_done()
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
                // suspends or resumes the write timer on a filter pause
                if self.timers.active.is_write() {
                    self.timers
                        .start_write(&self.io, self.codec.cfg.write_timeout);
                }
                return Poll::Pending;
            };
            match status {
                IoStatusUpdate::Timeout if self.timers.active.is_write() => {
                    if let Err(err) = self.write_timer_expired() {
                        Poll::Ready(self.ctl_peer_gone(Some(err)))
                    } else {
                        Poll::Pending
                    }
                }
                IoStatusUpdate::Timeout => Poll::Pending,
                IoStatusUpdate::WriteBackpressure => {
                    self.timers
                        .start_write(&self.io, self.codec.cfg.write_timeout);
                    Poll::Pending
                }
                IoStatusUpdate::PeerGone(e) => Poll::Ready(self.ctl_peer_gone(e)),
            }
        }
    }

    /// Fails the request payload, the payload stream ends with `err`.
    fn set_payload_error(&mut self, err: PayloadError) {
        if let Some((_, sender)) = self.payload.take() {
            sender.set_error(err);
        }
    }

    /// Process request's payload
    fn poll_request_payload(&mut self, cx: &mut Context<'_>) -> Poll<Option<State<F, B, Err>>> {
        let result = if let Some(err) = self.pending_payload_error.take() {
            Err(err)
        } else {
            ready!(self.poll_request_payload_inner(cx))
        };
        Poll::Ready(result.err().map(|err| self.ctl_payload_err(err)))
    }

    /// Process request's payload
    fn poll_request_payload_inner(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), PayloadFailure>> {
        // check if payload data is required
        let Some((_, sender)) = &self.payload else {
            return Poll::Ready(Ok(()));
        };

        match sender.poll_ready(cx) {
            Poll::Ready(bstream::Status::Ready) => {
                // read request payload
                let mut updated = false;
                let mut pending = None;
                self.release_write_timer();
                while let Some((payload_codec, sender)) = self.payload.as_mut() {
                    let result = if pending.is_some() {
                        self.io
                            .decode_item(payload_codec)
                            .map_err(RecvError::Decoder)
                    } else {
                        self.io.poll_recv_decode(payload_codec, cx)
                    };
                    let result = match result {
                        // the request payload is read regardless of write
                        // backpressure, and payload received together with a
                        // read timer wins over its expiry, the buffered input
                        // is decoded first
                        Err(err @ RecvError::WriteBackpressure) => {
                            pending = Some(err);
                            continue;
                        }
                        Err(err @ RecvError::Timeout) if !self.timers.active.is_write() => {
                            pending = Some(err);
                            continue;
                        }
                        Ok(decoded) if decoded.item.is_none() && pending.is_some() => {
                            // the decode attempt can consume bytes without an item
                            self.timers.payload_consumed(decoded.consumed as u32);
                            Err(pending.take().unwrap())
                        }
                        result => result,
                    };

                    let err = match result {
                        Ok(decoded) => {
                            self.timers
                                .payload_decoded(&self.io, decoded.consumed as u32);
                            match decoded.item {
                                Some(PayloadItem::Chunk(chunk)) => {
                                    updated = true;
                                    sender.feed_data(chunk);
                                    continue;
                                }
                                Some(PayloadItem::Trailers(trailers)) => {
                                    sender.feed_trailers(trailers);
                                    continue;
                                }
                                Some(PayloadItem::Eof) => {
                                    updated = true;
                                    sender.feed_eof();
                                    self.timers.payload_done(&self.io);
                                    self.payload = None;
                                    break;
                                }
                                None => {
                                    // the filter chain pauses reading, the
                                    // peer is not charged for it
                                    if self.io.is_read_filter_paused() {
                                        self.timers.suspend_read(&self.io);
                                    }
                                    break;
                                }
                            }
                        }
                        Err(RecvError::WriteBackpressure) => match self.poll_flush_timed(cx) {
                            Poll::Ready(Ok(())) => continue,
                            Poll::Ready(Err(err)) => {
                                self.set_payload_error(PayloadError::Incomplete(Some(
                                    clone_io_error(&err),
                                )));
                                PayloadFailure::PeerGone(Some(err))
                            }
                            Poll::Pending => {
                                self.timers.pause_payload(&self.io);
                                break;
                            }
                        },
                        Err(RecvError::Timeout) if self.timers.active.is_write() => {
                            if let Err(err) = self.write_timer_expired() {
                                PayloadFailure::PeerGone(Some(err))
                            } else {
                                continue;
                            }
                        }
                        Err(RecvError::Timeout) => {
                            if let Err(err) = self.handle_timeout() {
                                PayloadFailure::Protocol(err)
                            } else {
                                continue;
                            }
                        }
                        Err(RecvError::PeerGone(err)) => {
                            self.set_payload_error(PayloadError::Incomplete(
                                err.as_ref().map(clone_io_error),
                            ));
                            PayloadFailure::PeerGone(err)
                        }
                        Err(RecvError::Decoder(e)) => {
                            self.set_payload_error(PayloadError::Decode(e));
                            PayloadFailure::Protocol(ProtocolError::Decode(e))
                        }
                    };
                    return Poll::Ready(Err(err));
                }
                if updated {
                    Poll::Ready(Ok(()))
                } else {
                    Poll::Pending
                }
            }
            Poll::Pending => {
                self.timers.pause_payload(&self.io);
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
            self.timers
                .start_write(&self.io, self.codec.cfg.write_timeout);
            if !self.timers.active.is_write() {
                return Poll::Pending;
            }
            // the timer is reported through the status update
            match self.io.poll_status_update(cx) {
                Poll::Ready(IoStatusUpdate::Timeout) => {
                    if let Err(err) = self.write_timer_expired() {
                        return Poll::Ready(Err(err));
                    }
                }
                Poll::Pending if !self.io.is_wr_backpressure() => (),
                _ => return Poll::Pending,
            }
        }
    }

    /// Handles expiry of the write timer.
    ///
    /// Fails the request payload and returns the write timeout error if
    /// write backpressure is still enabled, otherwise stops the timer.
    /// An expiry during a filter write pause suspends the timer.
    fn write_timer_expired(&mut self) -> io::Result<()> {
        if self.io.is_wr_backpressure() && self.io.is_write_filter_paused() {
            // the filter chain paused writing before the dispatcher noticed
            self.timers
                .start_write(&self.io, self.codec.cfg.write_timeout);
            Ok(())
        } else if self.io.is_wr_backpressure() {
            log::trace!("{}: Write backpressure timeout", self.io.tag());
            self.set_payload_error(PayloadError::Io(write_timeout_error()));
            Err(write_timeout_error())
        } else {
            self.timers.stop_write(&self.io);
            Ok(())
        }
    }

    /// Stops the write timer if write backpressure has been disabled.
    fn release_write_timer(&mut self) {
        if self.timers.active.is_write() && !self.io.is_wr_backpressure() {
            self.timers.stop_write(&self.io);
        }
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

    /// Handles expiry of a read timer while a request head is awaited.
    ///
    /// `decoded` is the decode attempt of the input received together with
    /// the expiry, it did not produce a request head.
    fn headers_timeout(
        &mut self,
        decoded: &Decoded<(Request, PayloadType)>,
    ) -> Result<(), State<F, B, Err>> {
        if self.timers.active == Timer::Headers {
            self.handle_timeout().map_err(|err| {
                log::trace!("{}: Slow request timeout", self.io.tag());
                self.ctl_proto_err(err)
            })
        } else if self.start_headers_timer(
            self.codec.is_reading_hdrs(),
            (decoded.consumed + decoded.remains) as u32,
            decoded.remains as u32,
        ) {
            // a partial request head wins over keep-alive or client timeout
            Ok(())
        } else if self.timers.active == Timer::ClientTimeout {
            log::trace!("{}: Client timeout, no request", self.io.tag());
            Err(self.ctl_proto_err(ProtocolError::SlowRequestTimeout))
        } else {
            log::trace!("{}: Keep-alive timeout, close connection", self.io.tag());
            Err(self.ctl_keepalive(true))
        }
    }

    fn update_hdrs_timer(
        &mut self,
        decoded: &Decoded<(Request, PayloadType)>,
    ) -> Option<State<F, B, Err>> {
        let partial = self.codec.is_reading_hdrs() || decoded.remains != 0;
        let remains = decoded.remains as u32;

        if decoded.item.is_some() {
            // got parsed frame
            self.timers.reset(&self.io);
        } else if self.timers.active == Timer::Headers {
            // received new data but not enough for parsing complete frame
            self.timers.progress.remains = remains;
        } else if self.start_headers_timer(
            partial,
            (decoded.consumed as u32).saturating_add(remains),
            remains,
        ) {
            // the headers read rate bounds a partial request head, for the
            // first request it starts with the first received byte
        } else if self.timers.active == Timer::ClientTimeout {
            // only the headers read rate bounds the first request
        } else if partial || self.codec.keepalive() {
            // without a headers read rate the keep-alive timer bounds a
            // partial request head
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

    /// Starts request-head timing for a partial request head.
    ///
    /// Returns `false` if the head is not partial or no headers read rate
    /// is configured.
    fn start_headers_timer(&mut self, partial: bool, consumed: u32, remains: u32) -> bool {
        let rate = self.codec.cfg.headers_read_rate;
        if partial && rate.is_some() {
            self.timers.start_headers(&self.io, rate, consumed, remains);
            true
        } else {
            false
        }
    }

    /// Starts payload timing for the current request if it is not started
    /// yet, an expectation can be handled without `100 Continue`.
    fn start_payload_timer(&mut self) {
        if self.payload.is_some()
            && matches!(
                self.timers.active,
                Timer::Stopped | Timer::Idle | Timer::Write
            )
        {
            self.timers
                .start_payload(&self.io, self.codec.cfg.payload_read_rate);
        }
    }

    /// Handles a sent response: reports a pending disconnect, or reads
    /// the rest of the request payload, or the next request.
    fn response_done(&mut self) -> State<F, B, Err> {
        self.response_started = false;
        if let Some(st) = self.check_disconnect() {
            st
        } else if self.payload.is_some() {
            self.start_payload_timer();
            State::ReadPayload
        } else {
            self.next_request()
        }
    }

    /// Reads the next request, unless the last response closes the
    /// connection, then pipelined requests are not processed.
    fn next_request(&mut self) -> State<F, B, Err> {
        if self.codec.keepalive() {
            State::ReadRequest
        } else {
            log::trace!("{}: Connection is not persistent, close", self.io.tag());
            self.io.close();
            self.ctl_keepalive(false)
        }
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
        self.pending_payload_error
            .take()
            .map(|err| self.ctl_payload_err(err))
    }

    fn ctl_payload_err(&mut self, err: PayloadFailure) -> State<F, B, Err> {
        match err {
            PayloadFailure::Protocol(err) => self.ctl_proto_err(err),
            PayloadFailure::PeerGone(err) => self.ctl_peer_gone(err),
        }
    }

    /// Applies the control service acknowledgement.
    fn control_result(&mut self, result: ControlResult<F>) -> State<F, B, Err> {
        match result {
            ControlResult::Publish(req) => self.publish(req),
            // the response head is sent, an error response would be written
            // into the unfinished body
            ControlResult::ProtocolError(..) if self.response_started => {
                log::trace!("{}: Response is incomplete, close", self.io.tag());
                self.response_started = false;
                self.io.close();
                self.stop()
            }
            ControlResult::Response(res, body)
            | ControlResult::Error(res, body)
            | ControlResult::ProtocolError(res, body) => self.send_response(res, body.into()),
            ControlResult::Continue(req) => {
                let result = self.io.encode_slice(b"HTTP/1.1 100 Continue\r\n\r\n");
                if let Err(err) = result {
                    return self.ctl_peer_gone(Some(err));
                }
                self.start_payload_timer();
                if req.upgrade() {
                    self.ctl_upgrade(req)
                } else {
                    self.publish(req)
                }
            }
            ControlResult::Expect(req) => self.control(Control::expect(req)),
            ControlResult::ExpectFailed(res, body) => {
                self.set_disconnect(ServiceDisconnectReason::ExpectFailed);
                self.send_response(res, body.into())
            }
            ControlResult::Upgrade(req) => self.ctl_upgrade(req),
            ControlResult::UpgradeAck(req) => {
                self.set_disconnect(ServiceDisconnectReason::UpgradeHandled);
                self.publish(req)
            }
            ControlResult::UpgradeHandled => {
                self.ctl_svc_disconnect(ServiceDisconnectReason::UpgradeHandled)
            }
            ControlResult::UpgradeFailed(res, body) => {
                self.set_disconnect(ServiceDisconnectReason::UpgradeFailed);
                self.send_response(res, body.into())
            }
            ControlResult::Stop => self.stop(),
            ControlResult::Connect(_) => unreachable!(),
        }
    }

    /// Sends a control message, without a control service the default action
    /// is applied.
    fn control(&mut self, req: Control<F, Err>) -> State<F, B, Err> {
        if let Some(ctl) = &self.control {
            State::CallControl {
                fut: ctl.call_nowait(req),
            }
        } else {
            self.control_result(req.ack().result)
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
            self.control(req)
        }
    }

    fn check_disconnect(&mut self) -> Option<State<F, B, Err>> {
        match mem::replace(&mut self.disconnect, Disconnect::Sent) {
            Disconnect::None => {
                self.disconnect = Disconnect::None;
                None
            }
            Disconnect::Pending(reason) => Some(self.control(Control::svc_disconnect(reason))),
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
    use crate::http::h1::control::Reason;
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
            None,
            DispatcherConfig::default(),
        );

        h1.inner.timers.stop(&h1.inner.io);
        h1.inner.payload = Some((PayloadDecoder::length(4), Payload::create().0));
        h1.inner.start_payload_timer();
        h1.inner.timers.payload_decoded(&h1.inner.io, 2);
        assert_eq!(h1.inner.timers.progress.max_timeout, Seconds(5));
        assert!(h1.inner.handle_timeout().is_ok());
        assert_eq!(h1.inner.timers.progress.max_timeout, Seconds::ZERO);
        assert_eq!(h1.inner.io.timer_handle().remains(), Seconds(5));

        h1.inner.timers.pause_payload(&h1.inner.io);
        assert_eq!(h1.inner.timers.active, Timer::PayloadPaused);
        assert_eq!(h1.inner.timers.progress.period, Seconds(5));
        assert_eq!(h1.inner.timers.progress.max_timeout, Seconds::ZERO);

        h1.inner.timers.payload_decoded(&h1.inner.io, 2);
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
            None,
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
        // without a control service the disconnect is acknowledged in place
        assert!(matches!(h1.inner.ctl_peer_gone(None), State::Stop));
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
    ) -> (Dispatcher<Base, body::Body, io::Error>, Payload) {
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
            None,
            DispatcherConfig::default(),
        );
        let (tx, rx) = Payload::create();
        h1.inner.payload = Some((PayloadDecoder::length(4), tx));
        h1.inner.timers.stop(&h1.inner.io);
        h1.inner.timers.active = Timer::PayloadPaused;
        h1.inner.timers.progress.max_timeout = Seconds::ZERO;
        (h1, rx)
    }

    /// A detached payload reader is woken with an error when the dispatcher
    /// is dropped before the payload is complete.
    #[crate::rt_test]
    async fn test_dropped_dispatcher_fails_detached_payload() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        let result = Rc::new(RefCell::new(None));
        let result2 = result.clone();

        let mut h1 = Dispatcher::new(
            0,
            nio::Io::new(server, SharedCfg::new("SVC")),
            Pipeline::new(
                (),
                fn_service(move |mut req: Request| {
                    let result = result2.clone();
                    async move {
                        let mut pl = req.take_payload();
                        crate::rt::spawn(async move {
                            let mut last = None;
                            while let Some(item) = pl.recv().await {
                                last = Some(item);
                            }
                            *result.borrow_mut() = Some(last);
                        });
                        Ok::<_, io::Error>(Response::Ok().build())
                    }
                }),
            ),
            None,
            DispatcherConfig::default(),
        );

        client.write("POST / HTTP/1.1\r\nhost: a\r\ncontent-length: 10\r\n\r\npart");
        sleep(Millis(50)).await;
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        sleep(Millis(50)).await;
        assert!(result.borrow().is_none());

        drop(h1);
        sleep(Millis(50)).await;
        let res = result.borrow_mut().take();
        assert!(
            matches!(res, Some(Some(Err(PayloadError::Incomplete(None))))),
            "{res:?}"
        );
    }

    /// Resuming without budget left decodes the buffered payload first.
    #[crate::rt_test]
    async fn test_exhausted_payload_budget_decodes_buffered_data() {
        let (client, server) = IoTest::create();
        let (mut h1, mut rx) = exhausted_payload_h1(server, 1);

        client.write("test");
        sleep(Millis(50)).await;
        let res = lazy(|cx| h1.inner.poll_request_payload_inner(cx)).await;
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
        let res = lazy(|cx| h1.inner.poll_request_payload_inner(cx)).await;
        assert!(matches!(
            res,
            Poll::Ready(Err(PayloadFailure::Protocol(
                ProtocolError::SlowPayloadTimeout
            )))
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
        let res = lazy(|cx| h1.inner.poll_request_payload_inner(cx)).await;
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
        let res = lazy(|cx| h1.inner.poll_request_payload_inner(cx)).await;
        assert!(matches!(
            res,
            Poll::Ready(Err(PayloadFailure::Protocol(
                ProtocolError::SlowPayloadTimeout
            )))
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
        let res = lazy(|cx| h1.inner.poll_request_payload_inner(cx)).await;
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
            None,
            DispatcherConfig::default(),
        );

        client.write("POST / HTTP/1.1\r\nhost: localhost\r\ncontent-length: 4\r\n\r\n");
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
            None,
            DispatcherConfig::default(),
        );

        client.write("POST / HTTP/1.1\r\nhost: localhost\r\ncontent-length: 4\r\n\r\n");
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
            None,
            DispatcherConfig::default(),
        );

        client.write("GET /first HTTP/1.1\r\nhost: localhost\r\n\r\n");
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

    /// Bytes of a partial request head consumed by a decode attempt that ends
    /// with write backpressure count towards the headers read rate.
    #[crate::rt_test]
    async fn test_headers_rate_counts_bytes_decoded_during_write_backpressure() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(0);
        let config: SharedCfg = SharedCfg::new("SVC")
            .add(nio::IoConfig::new().set_write_buf(16))
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
            None,
            DispatcherConfig::default(),
        );

        // the response head of the first request exceeds the write watermark,
        // the decoder consumes the start line of the next request before
        // write backpressure is reported
        let partial = "GET /next HTTP/1.1\r\nhost: example.com";
        client.write(format!(
            "GET /first HTTP/1.1\r\nhost: localhost\r\n\r\n{partial}"
        ));
        sleep(Millis(50)).await;
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        assert!(h1.inner.io.is_wr_backpressure());
        assert_ne!(h1.inner.timers.active, Timer::Headers);

        client.remote_buffer_cap(1024);
        sleep(Millis(50)).await;
        assert!(client.read_any().starts_with(b"HTTP/1.1 200 OK\r\n"));
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        assert!(!h1.inner.io.is_wr_backpressure());
        assert_eq!(h1.inner.timers.active, Timer::Headers);
        assert_eq!(h1.inner.timers.progress.consumed, partial.len() as u32);

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
            None,
            DispatcherConfig::default(),
        )
    }

    /// Expiry of the keep-alive timer left armed while a request is processed
    /// does not close the connection.
    #[crate::rt_test]
    async fn test_idle_keepalive_expiry_is_ignored() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        let config: SharedCfg = SharedCfg::new("SVC")
            .add(
                HttpServiceConfig::new()
                    .set_client_timeout(Seconds::ZERO)
                    .set_keepalive(Seconds(1)),
            )
            .into();
        let mut h1 = Dispatcher::<_, body::Body, io::Error>::new(
            0,
            nio::Io::new(server, config),
            Pipeline::new(
                (),
                fn_service(async |req: Request| {
                    if req.path() == "/slow" {
                        sleep(Millis(1300)).await;
                    }
                    Ok::<_, io::Error>(Response::Ok().build())
                }),
            ),
            None,
            DispatcherConfig::default(),
        );

        client.write("GET / HTTP/1.1\r\nhost: localhost\r\n\r\n");
        sleep(Millis(50)).await;
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        sleep(Millis(50)).await;
        assert!(client.read_any().starts_with(b"HTTP/1.1 200 OK\r\n"));
        assert_eq!(h1.inner.timers.active, Timer::KeepAlive);

        client.write("GET /slow HTTP/1.1\r\nhost: localhost\r\n\r\n");
        sleep(Millis(50)).await;
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        assert_eq!(h1.inner.timers.active, Timer::Idle);

        // the armed keep-alive timer expires while the service is busy
        sleep(Millis(1500)).await;
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        sleep(Millis(50)).await;
        assert!(client.read_any().starts_with(b"HTTP/1.1 200 OK\r\n"));
        assert!(h1.inner.io.is_active());
        assert_eq!(h1.inner.timers.active, Timer::KeepAlive);

        client.write("GET / HTTP/1.1\r\nhost: localhost\r\n\r\n");
        sleep(Millis(50)).await;
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        sleep(Millis(50)).await;
        assert!(client.read_any().starts_with(b"HTTP/1.1 200 OK\r\n"));

        client.close().await;
        assert!(poll_fn(|cx| Pin::new(&mut h1).poll(cx)).await.is_ok());
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

        client.write("GET / HTTP/1.1\r\nhost: localhost\r\n");
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

        client.write("GET /first HTTP/1.1\r\nhost: localhost\r\n\r\nGET /next HTTP/1.1\r\nhost: localhost\r\n");
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
            None,
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

        let partial = "GET / HTTP/1.1\r\nhost: localhost\r\n";
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
        client.write("GET / HTTP/1.1\r\nhost: localhost\r\n");
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
            None,
            DispatcherConfig::default(),
        );

        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        assert!(h1.inner.io.is_active());

        client.write("GET / HTTP/1.1\r\nhost: localhost\r\n\r\n");
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
            None,
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
            None,
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
            Some(Pipeline::new(
                (),
                fn_service(async move |req: Control<_, _>| {
                    if let Control::Request(_) = req {
                        data2.set(true);
                    }
                    Ok::<_, DispatchError>(req.ack())
                }),
            )),
            DispatcherConfig::default(),
        );
        sleep(Millis(50)).await;
        let _ = lazy(|cx| Pin::new(&mut h1).poll(cx)).await;
        sleep(Millis(50)).await;

        client.local_buffer(|buf| assert_eq!(&buf[..15], b"HTTP/1.1 200 OK"));
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

        client.write("GET /test1 HTTP/1.1\r\nhost: localhost\r\n\r\n");

        let mut buf = BytesMut::from(&client.read().await.unwrap()[..]);
        assert!(load(&mut decoder, &mut buf).status.is_success());
        assert!(!client.is_server_dropped());

        client.write("GET /test2 HTTP/1.1\r\nhost: localhost\r\n\r\n");
        client.write("GET /test3 HTTP/1.1\r\nhost: localhost\r\n\r\n");

        let mut buf = BytesMut::from(&client.read().await.unwrap()[..]);
        assert!(load(&mut decoder, &mut buf).status.is_success());
        assert!(load(&mut decoder, &mut buf).status.is_success());
        assert!(decoder.decode(&mut buf).unwrap().is_none());
        assert!(!client.is_server_dropped());

        client.close().await;
        assert!(client.is_server_dropped());
    }

    #[crate::rt_test]
    async fn test_pipeline_after_close() {
        for (req, res) in [
            (
                "GET /test1 HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n",
                "HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n",
            ),
            (
                "GET /test1 HTTP/1.0\r\n\r\n",
                "HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n",
            ),
            (
                "POST /test1 HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\ncontent-length: 4\r\n\r\nbody",
                "HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n",
            ),
        ] {
            let (client, server) = IoTest::create();
            client.remote_buffer_cap(4096);
            let calls = Rc::new(Cell::new(0));
            let calls2 = calls.clone();
            spawn_h1(server, move |mut req: Request| {
                calls2.set(calls2.get() + 1);
                async move {
                    let mut pl = req.take_payload();
                    while let Some(item) = crate::util::stream_recv(&mut pl).await {
                        item.unwrap();
                    }
                    Ok::<_, io::Error>(Response::Ok().build())
                }
            });

            // the next request is pipelined behind a non-persistent request
            client.write(format!(
                "{req}GET /test2 HTTP/1.1\r\nhost: localhost\r\n\r\n"
            ));
            sleep(Millis(100)).await;

            let buf = client.read_any();
            assert!(buf.starts_with(res.as_bytes()), "{req:?} {buf:?}");
            assert_eq!(
                buf.windows(7).filter(|w| w == b"HTTP/1.").count(),
                1,
                "{req:?}"
            );
            assert_eq!(calls.get(), 1, "{req:?}");
            assert!(client.is_server_dropped(), "{req:?}");
        }
    }

    #[crate::rt_test]
    async fn test_response_without_body_is_framed() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(4096);
        spawn_h1(server, move |req: Request| async move {
            if req.path() == "/test1" {
                Ok::<_, io::Error>(Response::Ok().body(body::Body::None))
            } else {
                Ok(Response::Ok().body("next"))
            }
        });

        client.write(
            "GET /test1 HTTP/1.1\r\nhost: a\r\n\r\n\
             GET /test2 HTTP/1.1\r\nhost: a\r\n\r\n",
        );
        sleep(Millis(100)).await;

        let buf = client.read_any();
        let data = String::from_utf8(buf.to_vec()).unwrap();
        let (first, second) = data.split_at(data.rfind("HTTP/1.1 200 OK").unwrap());
        assert!(first.starts_with("HTTP/1.1 200 OK\r\n"), "{data:?}");
        assert!(first.contains("\r\ncontent-length: 0\r\n"), "{data:?}");
        assert!(first.ends_with("\r\n\r\n"), "{data:?}");
        assert!(second.contains("\r\ncontent-length: 4\r\n"), "{data:?}");
        assert!(second.ends_with("\r\n\r\nnext"), "{data:?}");
        assert!(!client.is_server_dropped());
    }

    #[crate::rt_test]
    async fn test_informational_response_is_replaced() {
        for status in [
            StatusCode::CONTINUE,
            StatusCode::PROCESSING,
            StatusCode::EARLY_HINTS,
        ] {
            let (client, server) = IoTest::create();
            client.remote_buffer_cap(4096);
            spawn_h1(server, move |req: Request| async move {
                if req.path() == "/test1" {
                    Ok::<_, io::Error>(Response::builder(status).body("interim"))
                } else {
                    Ok(Response::Ok().build())
                }
            });

            client.write(
                "GET /test1 HTTP/1.1\r\nhost: a\r\n\r\n\
                 GET /test2 HTTP/1.1\r\nhost: a\r\n\r\n",
            );
            sleep(Millis(100)).await;

            let buf = client.read_any();
            assert!(
                buf.starts_with(b"HTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\n"),
                "{status} {buf:?}"
            );
            assert_eq!(
                buf.windows(9).filter(|w| w == b"HTTP/1.1 ").count(),
                2,
                "{status} {buf:?}"
            );
            assert!(!buf.windows(7).any(|w| w == b"interim"), "{status} {buf:?}");
            assert!(
                buf.windows(15).any(|w| w == b"HTTP/1.1 200 OK"),
                "{status} {buf:?}"
            );
        }
    }

    #[crate::rt_test]
    async fn test_upgrade_without_connection_option() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(4096);
        let seen = Rc::new(RefCell::new(Vec::new()));
        let seen2 = seen.clone();
        spawn_h1(server, move |req: Request| {
            seen2
                .borrow_mut()
                .push((req.path().to_string(), req.upgrade()));
            async { Ok::<_, io::Error>(Response::Ok().build()) }
        });

        client.write(
            "GET /test1 HTTP/1.1\r\nhost: a\r\nupgrade: websocket\r\n\r\n\
             GET /test2 HTTP/1.1\r\nhost: a\r\nconnection: upgrade\r\n\r\n\
             GET /test3 HTTP/1.1\r\nhost: a\r\n\r\n",
        );
        sleep(Millis(100)).await;

        let buf = client.read_any();
        assert_eq!(
            buf.windows(15).filter(|w| w == b"HTTP/1.1 200 OK").count(),
            3,
            "{buf:?}"
        );
        assert_eq!(
            *seen.borrow(),
            [
                ("/test1".to_string(), false),
                ("/test2".to_string(), false),
                ("/test3".to_string(), false)
            ]
        );
    }

    #[crate::rt_test]
    async fn test_http10_expect_and_upgrade_ignored() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(4096);
        let seen = Rc::new(RefCell::new(Vec::new()));
        let seen2 = seen.clone();
        spawn_h1(server, move |mut req: Request| {
            seen2
                .borrow_mut()
                .push((req.path().to_string(), req.upgrade()));
            async move {
                let mut pl = req.take_payload();
                while let Some(item) = crate::util::stream_recv(&mut pl).await {
                    item.unwrap();
                }
                Ok::<_, io::Error>(Response::Ok().build())
            }
        });

        client.write(
            "POST /test1 HTTP/1.0\r\nconnection: keep-alive\r\n\
             expect: 100-continue\r\ncontent-length: 4\r\n\r\nbody\
             GET /test2 HTTP/1.0\r\nconnection: keep-alive, upgrade\r\n\
             upgrade: websocket\r\n\r\n\
             GET /test3 HTTP/1.0\r\n\r\n",
        );
        sleep(Millis(100)).await;

        let buf = client.read_any();
        // no interim response for an HTTP/1.0 client
        assert!(buf.starts_with(b"HTTP/1.1 200 OK\r\n"), "{buf:?}");
        assert!(!buf.windows(3).any(|w| w == b"100"), "{buf:?}");
        assert_eq!(
            *seen.borrow(),
            [
                ("/test1".to_string(), false),
                ("/test2".to_string(), false),
                ("/test3".to_string(), false)
            ]
        );
    }

    #[crate::rt_test]
    async fn test_invalid_host_rejected() {
        for req in [
            "GET /test HTTP/1.1\r\n\r\n",
            "GET /test HTTP/1.1\r\nhost: a\r\nhost: b\r\n\r\n",
            "GET /test HTTP/1.1\r\nhost: user@a\r\n\r\n",
        ] {
            let (client, server) = IoTest::create();
            client.remote_buffer_cap(4096);
            let calls = Rc::new(Cell::new(0));
            let calls2 = calls.clone();
            spawn_h1(server, move |_| {
                calls2.set(calls2.get() + 1);
                async { Ok::<_, io::Error>(Response::Ok().build()) }
            });

            client.write(req);
            sleep(Millis(100)).await;

            let buf = client.read_any();
            assert!(
                buf.starts_with(b"HTTP/1.1 400 Bad Request\r\n"),
                "{req:?} {buf:?}"
            );
            assert_eq!(calls.get(), 0, "{req:?}");
            assert!(client.is_server_dropped(), "{req:?}");
        }
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

        client.write("GET /test1 HTTP/1.1\r\nhost: localhost\r\ncontent-length: 5\r\n\r\n");
        sleep(Millis(50)).await;
        client.write("xxxxx");

        let mut buf = BytesMut::from(&client.read().await.unwrap()[..]);
        assert!(load(&mut decoder, &mut buf).status.is_success());
        assert!(!client.is_server_dropped());

        client.write("GET /test2 HTTP/1.1\r\nhost: localhost\r\n\r\n");

        let mut buf = BytesMut::from(&client.read().await.unwrap()[..]);
        assert!(load(&mut decoder, &mut buf).status.is_success());
        assert!(decoder.decode(&mut buf).unwrap().is_none());
        assert!(!client.is_server_dropped());

        client.close().await;
        assert!(client.is_server_dropped());
    }

    #[crate::rt_test]
    /// The rest of a payload that is still held by the application after the
    /// response is sent must not be decoded as the next request.
    async fn test_payload_after_response_is_not_a_request() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(4096);
        let calls = Rc::new(Cell::new(0));
        let held = Rc::new(RefCell::new(None));
        let (calls2, held2) = (calls.clone(), held.clone());

        spawn_h1(server, move |mut req: Request| {
            calls2.set(calls2.get() + 1);
            *held2.borrow_mut() = Some(req.take_payload());
            async { Ok::<_, io::Error>(Response::Ok().build()) }
        });

        let smuggled = "GET /s HTTP/1.1\r\nhost: a\r\n\r\n";
        client.write(format!(
            "POST / HTTP/1.1\r\nhost: localhost\r\ncontent-length: {}\r\n\r\naaaaa",
            5 + smuggled.len()
        ));
        let _ = client.read().await.unwrap();
        sleep(Millis(50)).await;
        client.write(smuggled);
        sleep(Millis(100)).await;
        assert_eq!(calls.get(), 1, "payload bytes dispatched as a request");

        let mut pl = held.borrow_mut().take().unwrap();
        let mut body = BytesMut::new();
        while let Some(chunk) = stream_recv(&mut pl).await {
            body.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(&body[..], format!("aaaaa{smuggled}").as_bytes());

        client.write("GET /next HTTP/1.1\r\nhost: localhost\r\n\r\n");
        let _ = client.read().await.unwrap();
        assert_eq!(calls.get(), 2);
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

        client.write("GET /test HTTP/1.1\r\nhost: localhost\r\n\r\n");

        let mut buf = BytesMut::from(&client.read().await.unwrap()[..]);
        assert!(load(&mut decoder, &mut buf).status.is_success());
        assert!(!client.is_server_dropped());

        client.write("GET /test HTTP/1.1\r\nhost: localhost\r\n\r\n");
        client.write("GET /test HTTP/1.1\r\nhost: localhost\r\n\r\n");
        sleep(Millis(50)).await;
        client.write("GET /test HTTP/1.1\r\nhost: localhost\r\n\r\n");

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
        client.write("GET /test HTTP/1.1\r\nhost: localhost\r\n\r\n");
        client.write("GET /test HTTP/1.1\r\nhost: localhost\r\n\r\n");
        client.write("GET /test HTTP/1.1\r\nhost: localhost\r\n\r\n");
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
                            .set_read_buf(15 * 1024, 1024)
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
        client.write("GET /test HTTP/1.1\r\nhost: localhost\r\nContent-Length: ");
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

        client.write("GET /test HTTP/1.1\r\nhost: localhost\r\nContent-Length: 1048576\r\n\r\n");
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
            None,
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

        client.write("GET / HTTP/1.1\r\nhost: localhost\r\n\r\n");
        sleep(Millis(50)).await;
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        assert!(h1.inner.io.is_wr_backpressure());
        assert_eq!(h1.inner.timers.active, Timer::Write);

        let res = timeout(Millis(5000), poll_fn(|cx| Pin::new(&mut h1).poll(cx))).await;
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

        client.write("POST / HTTP/1.1\r\nhost: localhost\r\ncontent-length: 4\r\n\r\nb");
        let res = timeout(Millis(5000), poll_fn(|cx| Pin::new(&mut h1).poll(cx))).await;
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
        h1.inner.pending_payload_error =
            Some(PayloadFailure::Protocol(ProtocolError::SlowPayloadTimeout));

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

        client.write("GET / HTTP/1.1\r\nhost: localhost\r\n\r\n");
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

        client.write("POST / HTTP/1.1\r\nhost: localhost\r\ncontent-length: 4\r\n\r\nb");
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
            None,
            DispatcherConfig::default(),
        );

        client.write("POST / HTTP/1.1\r\nhost: localhost\r\ncontent-length: 4\r\n\r\nb");
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

    /// The request payload is read while write backpressure is active.
    #[crate::rt_test]
    async fn test_payload_read_during_write_backpressure() {
        let body = Rc::new(RefCell::new(BytesMut::new()));
        let body2 = body.clone();
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(0);

        let mut h1 = Dispatcher::new(
            0,
            nio::Io::new(server, SharedCfg::new("SVC")),
            Pipeline::new(
                (),
                fn_service(move |mut req: Request| {
                    let body = body2.clone();
                    let mut pl = req.take_payload();
                    crate::rt::spawn(async move {
                        while let Some(Ok(chunk)) = stream_recv(&mut pl).await {
                            body.borrow_mut().extend_from_slice(&chunk);
                        }
                    });
                    async { Ok::<_, io::Error>(Response::Ok().body("x".repeat(128 * 1024))) }
                }),
            ),
            None,
            DispatcherConfig::default(),
        );

        client.write("POST / HTTP/1.1\r\nhost: localhost\r\ncontent-length: 4\r\n\r\nb");
        sleep(Millis(50)).await;
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        assert!(h1.inner.io.is_wr_backpressure());

        client.write("ody");
        sleep(Millis(50)).await;
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        sleep(Millis(50)).await;
        assert!(h1.inner.io.is_wr_backpressure());
        assert_eq!(&body.borrow()[..], b"body");

        client.remote_buffer_cap(256 * 1024);
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
        client.write("GET /test HTTP/1.1\r\nhost: localhost\r\n\r\n");
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

        client.write("GET /test HTTP/1.1\r\nhost: localhost\r\n\r\n");
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
            Some(Pipeline::new(
                (),
                fn_service(async move |msg: Control<_, _>| {
                    if let Control::Disconnect(Reason::PeerGone(err)) = &msg {
                        assert!(err.get_ref().is_none());
                        disconnects2.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok::<_, DispatchError>(msg.ack())
                }),
            )),
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
            Some(Pipeline::new(
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
            )),
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
            None,
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
            None,
            DispatcherConfig::default(),
        );

        client.write("POST / HTTP/1.1\r\nhost: localhost\r\ntransfer-encoding: chunked\r\n\r\n");
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
            Some(Pipeline::new(
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
            )),
            DispatcherConfig::default(),
        );
        crate::rt::spawn(h1);

        client.write("POST / HTTP/1.1\r\nhost: localhost\r\ncontent-length: 4\r\nexpect: 100-continue\r\n\r\n");
        // an expired payload timer would respond with an error or close
        let buf = client.read().await.unwrap();
        assert_eq!(&buf[..], b"HTTP/1.1 100 Continue\r\n\r\n");

        client.write("test");
        let buf = client.read().await.unwrap();
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
            Some(Pipeline::new(
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
            )),
            DispatcherConfig::default(),
        );

        client.write("POST / HTTP/1.1\r\nhost: localhost\r\ncontent-length: 4\r\nexpect: 100-continue\r\n\r\n");
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
            Some(Pipeline::new(
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
            )),
            DispatcherConfig::default(),
        );

        client.write("POST / HTTP/1.1\r\nhost: localhost\r\ncontent-length: 10\r\n\r\nbody");
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
            Some(Pipeline::new(
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
            )),
            DispatcherConfig::default(),
        );

        client.write(
            "POST / HTTP/1.1\r\nhost: localhost\r\ntransfer-encoding: chunked\r\n\r\ninvalid\r\n",
        );
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
            Some(Pipeline::new(
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
            )),
            DispatcherConfig::default(),
        );

        client.write("POST / HTTP/1.1\r\nhost: localhost\r\ncontent-length: 10\r\n\r\nbody");
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

        client.write("POST / HTTP/1.1\r\nhost: localhost\r\ncontent-length: 10\r\n\r\nbody");
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

        client.write("POST / HTTP/1.1\r\nhost: localhost\r\ncontent-length: 10\r\n\r\nbody");
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

        client.write(
            "POST / HTTP/1.1\r\nhost: localhost\r\ntransfer-encoding: chunked\r\n\r\ninvalid\r\n",
        );
        assert!(poll_fn(|cx| Pin::new(&mut h1).poll(cx)).await.is_ok());
        sleep(Millis(50)).await;
        assert_eq!(mark.load(Ordering::Relaxed), 1);
    }

    #[crate::rt_test]
    async fn test_service_error() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(4096);
        client.write("GET /test HTTP/1.1\r\nhost: localhost\r\ncontent-length:512\r\n\r\n");

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
            Some(Pipeline::new(
                (),
                fn_service(async move |msg: Control<_, _>| {
                    if let Control::Disconnect(Reason::ProtocolError(ref err)) = msg
                        && matches!(err.get_ref(), ProtocolError::SlowPayloadTimeout)
                    {
                        err_mark2.store(err_mark2.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
                    }
                    Ok::<_, DispatchError>(msg.ack())
                }),
            )),
            DispatcherConfig::default(),
        );
        crate::rt::spawn(disp);

        client.write("GET /test HTTP/1.1\r\nhost: localhost\r\nContent-Length: 1048576\r\n\r\n");
        sleep(Millis(50)).await;

        // send partial data to server, 1200 bytes per second exceeds the
        // configured rate in every period
        for _ in 0..20 {
            let random_bytes: Vec<u8> = (0..300).map(|_| rand::random::<u8>()).collect();
            client.write(random_bytes);
            sleep(Millis(250)).await;
            if err_mark.load(Ordering::Relaxed) != 0 {
                break;
            }
        }
        // The first period exceeds the configured rate and earns one
        // extension; the two-second maximum then terminates the payload.
        // Each one-second period lasts about one and less than two seconds
        // (a timer may expire early by the age of the cached time), so the
        // payload ends after about 2 to 4 seconds. Without the extension it
        // would end after about one second, with at most 1500 bytes.
        let received = mark.load(Ordering::Relaxed);
        assert!((1800..=5400).contains(&received), "received: {received}");
        assert_eq!(err_mark.load(Ordering::Relaxed), 1);
    }

    #[crate::rt_test]
    /// A payload dropped while the service is running closes the connection,
    /// the response announces it.
    async fn test_payload_dropped_before_response_closes() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(4096);

        spawn_h1(server, async |mut req: Request| {
            drop(req.take_payload());
            sleep(Millis(100)).await;
            Ok::<_, io::Error>(Response::Ok().body("TEST"))
        });

        client.write("POST /test HTTP/1.1\r\nhost: localhost\r\ncontent-length: 512\r\n\r\n");
        sleep(Millis(50)).await;
        client.write("aaaaa");

        let buf = client.read().await.unwrap();
        assert!(
            buf.starts_with(b"HTTP/1.1 200 OK\r\ncontent-length: 4\r\nconnection: close\r\n"),
            "{buf:?}"
        );
        sleep(Millis(50)).await;
        assert!(client.is_server_dropped());
    }

    #[crate::rt_test]
    /// A peer disconnect stops the dispatcher while the response body is
    /// pending, the body is dropped.
    async fn test_pending_body_dropped_on_peer_gone() {
        struct Stream(Rc<Cell<bool>>, bool);
        impl Drop for Stream {
            fn drop(&mut self) {
                self.0.set(true);
            }
        }
        impl body::MessageBody for Stream {
            fn size(&self) -> body::BodySize {
                body::BodySize::Stream
            }
            fn poll_next_chunk(
                &mut self,
                _: &mut Context<'_>,
            ) -> Poll<Option<Result<Bytes, Rc<dyn error::Error>>>> {
                if self.1 {
                    Poll::Pending
                } else {
                    self.1 = true;
                    Poll::Ready(Some(Ok(Bytes::from_static(b"data"))))
                }
            }
        }

        for delay in [false, true] {
            let (client, server) = IoTest::create();
            client.remote_buffer_cap(4096);
            let dropped = Rc::new(Cell::new(false));
            let dropped2 = dropped.clone();
            spawn_h1(server, move |_| {
                let d = dropped2.clone();
                async move {
                    if delay {
                        sleep(Millis(20)).await;
                    }
                    Ok::<_, io::Error>(Response::Ok().message_body(Stream(d, false)))
                }
            });
            client.write("GET /test HTTP/1.1\r\nhost: localhost\r\n\r\n");
            let _ = client.read().await.unwrap();
            sleep(Millis(50)).await;
            client.read_error(io::Error::from(io::ErrorKind::ConnectionReset));
            sleep(Millis(100)).await;
            assert!(dropped.get(), "delay={delay}");
        }
    }

    /// A client half-close drops an idle response body, unless half-close is
    /// enabled or pipelined requests are still buffered.
    #[crate::rt_test]
    async fn test_pending_body_dropped_on_half_close() {
        struct Stream(Rc<Cell<bool>>, bool);
        impl Drop for Stream {
            fn drop(&mut self) {
                self.0.set(true);
            }
        }
        impl body::MessageBody for Stream {
            fn size(&self) -> body::BodySize {
                body::BodySize::Stream
            }
            fn poll_next_chunk(
                &mut self,
                _: &mut Context<'_>,
            ) -> Poll<Option<Result<Bytes, Rc<dyn error::Error>>>> {
                if self.1 {
                    Poll::Pending
                } else {
                    self.1 = true;
                    Poll::Ready(Some(Ok(Bytes::from_static(b"data"))))
                }
            }
        }

        for (half_close, pipelined, delay) in [
            (false, false, false),
            (false, false, true),
            (true, false, false),
            (false, true, false),
        ] {
            let (client, server) = IoTest::create();
            client.remote_buffer_cap(4096);
            let cfg: SharedCfg = SharedCfg::new("DBG")
                .add(HttpServiceConfig::new().set_half_close(half_close))
                .into();
            let dropped = Rc::new(Cell::new(false));
            let dropped2 = dropped.clone();
            let svc = move |_| {
                let d = dropped2.clone();
                async move {
                    if delay {
                        sleep(Millis(50)).await;
                    }
                    Ok::<_, io::Error>(Response::Ok().message_body(Stream(d, false)))
                }
            };
            crate::rt::spawn(Dispatcher::new(
                0,
                nio::Io::new(server, cfg),
                Pipeline::new((), fn_service(svc).map(Into::into)),
                None,
                DispatcherConfig::default(),
            ));

            client.write("GET /test HTTP/1.1\r\nhost: localhost\r\n\r\n");
            if pipelined {
                client.write("GET /test HTTP/1.1\r\nhost: localhost\r\n\r\n");
            }
            if !delay {
                let _ = client.read().await.unwrap();
            }
            client.close().await;
            sleep(Millis(150)).await;
            let case = format!("half_close={half_close} pipelined={pipelined} delay={delay}");
            if half_close || pipelined {
                assert!(!dropped.get(), "{case}");
                assert!(!client.is_server_dropped(), "{case}");
            } else {
                assert!(dropped.get(), "{case}");
                assert!(client.is_server_dropped(), "{case}");
            }
        }
    }

    /// A response body is not polled after its declared length is sent, or
    /// for a response without a body.
    #[crate::rt_test]
    async fn test_body_not_polled_after_length() {
        struct Stream(Rc<Cell<usize>>, body::BodySize);
        impl body::MessageBody for Stream {
            fn size(&self) -> body::BodySize {
                self.1
            }
            fn poll_next_chunk(
                &mut self,
                _: &mut Context<'_>,
            ) -> Poll<Option<Result<Bytes, Rc<dyn error::Error>>>> {
                self.0.set(self.0.get() + 1);
                if self.0.get() > 10_000 {
                    Poll::Ready(None)
                } else {
                    Poll::Ready(Some(Ok(Bytes::from_static(b"data"))))
                }
            }
        }

        for (method, size, polls) in [
            ("GET", body::BodySize::Sized(6), 2),
            ("HEAD", body::BodySize::Sized(6), 0),
            ("HEAD", body::BodySize::Stream, 0),
        ] {
            let (client, server) = IoTest::create();
            client.remote_buffer_cap(1024 * 1024);
            let count = Rc::new(Cell::new(0));
            let count2 = count.clone();
            spawn_h1(server, move |_| {
                let c = count2.clone();
                async move { Ok::<_, io::Error>(Response::Ok().message_body(Stream(c, size))) }
            });

            let req = format!("{method} /test HTTP/1.1\r\nhost: localhost\r\n\r\n");
            client.write(&req);
            sleep(Millis(50)).await;
            let case = format!("{method} {size:?}");
            assert_eq!(count.get(), polls, "{case}");
            let buf = client.read_any();
            if method == "GET" {
                assert!(buf.ends_with(b"\r\n\r\ndatada"), "{case} {buf:?}");
            } else {
                assert!(buf.ends_with(b"\r\n\r\n"), "{case} {buf:?}");
            }

            // the connection stays persistent
            client.write(&req);
            sleep(Millis(50)).await;
            assert_eq!(count.get(), polls * 2, "{case}");
            assert!(
                client.read_any().starts_with(b"HTTP/1.1 200 OK\r\n"),
                "{case}"
            );
            assert!(!client.is_server_dropped(), "{case}");
        }
    }

    /// A response with a declared length is complete once its last byte is
    /// sent, the body is not polled for its end.
    #[crate::rt_test]
    async fn test_sized_body_complete_without_end() {
        struct Stream(bool);
        impl body::MessageBody for Stream {
            fn size(&self) -> body::BodySize {
                body::BodySize::Sized(4)
            }
            fn poll_next_chunk(
                &mut self,
                _: &mut Context<'_>,
            ) -> Poll<Option<Result<Bytes, Rc<dyn error::Error>>>> {
                // the stream never ends after its last chunk
                if self.0 {
                    Poll::Pending
                } else {
                    self.0 = true;
                    Poll::Ready(Some(Ok(Bytes::from_static(b"data"))))
                }
            }
        }

        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024 * 1024);
        spawn_h1(server, async |_| {
            Ok::<_, io::Error>(Response::Ok().message_body(Stream(false)))
        });

        let req = "GET /test HTTP/1.1\r\nhost: localhost\r\n\r\n";
        client.write(format!("{req}{req}"));
        sleep(Millis(100)).await;
        let buf = client.read_any();
        assert_eq!(
            buf.windows(4).filter(|w| w == b"data").count(),
            2,
            "{buf:?}"
        );
        assert!(!client.is_server_dropped());
    }

    /// The end of a chunked response body is encoded.
    #[crate::rt_test]
    async fn test_chunked_body_terminated() {
        use futures_util::stream;

        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024 * 1024);
        spawn_h1(server, async |_| {
            Ok::<_, io::Error>(Response::Ok().streaming(stream::iter([Ok::<_, io::Error>(
                Bytes::from_static(b"data"),
            )])))
        });

        client.write("GET /test HTTP/1.1\r\nhost: localhost\r\n\r\n");
        sleep(Millis(50)).await;
        let buf = client.read_any();
        assert!(buf.ends_with(b"\r\n\r\n4\r\ndata\r\n0\r\n\r\n"), "{buf:?}");
    }

    /// The last chunk of a response with a declared length is flushed before
    /// the connection is closed, it is not left to the shutdown timeout.
    #[crate::rt_test]
    async fn test_sized_body_flushed_before_close() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(0);
        let config: SharedCfg = SharedCfg::new("SVC")
            .add(nio::IoConfig::new().set_shutdown_timeout(Seconds(1)))
            .into();
        crate::rt::spawn(Dispatcher::new(
            0,
            nio::Io::new(server, config),
            Pipeline::new(
                (),
                fn_service(async |_| {
                    Ok::<_, io::Error>(Response::Ok().body(Bytes::from(vec![b'x'; 128 * 1024])))
                }),
            ),
            None,
            DispatcherConfig::default(),
        ));

        client.write("GET /test HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n");
        sleep(Millis(1500)).await;
        assert!(!client.is_server_dropped());

        client.remote_buffer_cap(1024 * 1024);
        let mut buf = BytesMut::new();
        loop {
            let data = client.read().await.unwrap();
            if data.is_empty() {
                break;
            }
            buf.extend_from_slice(&data);
        }
        assert!(buf.ends_with(&[b'x'; 1024]), "{}", buf.len());
        assert!(buf.len() > 128 * 1024);
    }

    #[crate::rt_test]
    async fn test_unconsumed_payload() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(4096);
        client.write("GET /test HTTP/1.1\r\nhost: localhost\r\ncontent-length:512\r\n\r\n");

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

    type Events = Rc<RefCell<Vec<String>>>;

    /// Creates a dispatcher with a control service that records every event
    /// and answers it with `f`.
    fn ctl_h1<S, C>(
        server: IoTest,
        events: &Events,
        svc: S,
        ctl: C,
    ) -> Dispatcher<Base, body::Body, io::Error>
    where
        S: AsyncFn(Request) -> Result<Response, io::Error> + 'static,
        C: Fn(Control<Base, io::Error>) -> Result<ControlAck<Base>, DispatchError> + 'static,
    {
        let events = events.clone();
        let svc = Rc::new(svc);
        let ctl = Rc::new(ctl);
        Dispatcher::new(
            0,
            nio::Io::new(server, SharedCfg::default()),
            Pipeline::new(
                (),
                fn_service(move |req: Request| {
                    let svc = svc.clone();
                    async move { svc(req).await }
                }),
            ),
            Some(Pipeline::new(
                (),
                fn_service(move |msg: Control<Base, io::Error>| {
                    let name = match &msg {
                        Control::Connect(_) => "connect".to_string(),
                        Control::Request(_) => "request".to_string(),
                        Control::Upgrade(_) => "upgrade".to_string(),
                        Control::Expect(_) => "expect".to_string(),
                        Control::Disconnect(Reason::Service(r)) => {
                            format!("service:{:?}", r.reason())
                        }
                        Control::Disconnect(Reason::Error(_)) => "error".to_string(),
                        Control::Disconnect(Reason::ProtocolError(e)) => match e.get_ref() {
                            ProtocolError::ResponsePayload(_) => "response-payload".to_string(),
                            ProtocolError::Decode(_) => "decode".to_string(),
                            e => format!("protocol:{e}"),
                        },
                        Control::Disconnect(Reason::PeerGone(p)) => {
                            format!("peer-gone:{}", p.get_ref().is_some())
                        }
                        Control::Disconnect(Reason::KeepAlive(k)) => {
                            format!("keepalive:{}", k.is_enabled())
                        }
                    };
                    events.borrow_mut().push(name);
                    let res = ctl(msg);
                    async move { res }
                }),
            )),
            DispatcherConfig::default(),
        )
    }

    fn events(events: &Events) -> Vec<String> {
        events.borrow().clone()
    }

    /// A control service error stops the dispatcher with that error.
    #[crate::rt_test]
    async fn test_control_error_stops_dispatcher() {
        use crate::error::IntoFailure;

        let (client, server) = IoTest::create();
        client.remote_buffer_cap(4096);
        let ev = Events::default();
        let calls = Rc::new(Cell::new(0));
        let calls2 = calls.clone();
        let mut h1 = ctl_h1(
            server,
            &ev,
            async move |_| {
                calls2.set(calls2.get() + 1);
                Ok(Response::Ok().build())
            },
            |msg| match msg {
                Control::Request(_) => Err(DispatchError::Control(io::Error::other("ctl").fail())),
                msg => Ok(msg.ack()),
            },
        );

        client.write("GET / HTTP/1.1\r\nhost: a\r\n\r\n");
        let res = timeout(Millis(1000), poll_fn(|cx| Pin::new(&mut h1).poll(cx))).await;
        assert!(matches!(res, Ok(Err(DispatchError::Control(_)))));
        assert_eq!(calls.get(), 0);
        assert_eq!(events(&ev), ["request"]);
    }

    /// A rejected upgrade sends the response and closes the connection.
    #[crate::rt_test]
    async fn test_upgrade_failed() {
        for with_err in [false, true] {
            let (client, server) = IoTest::create();
            client.remote_buffer_cap(4096);
            let ev = Events::default();
            let calls = Rc::new(Cell::new(0));
            let calls2 = calls.clone();
            let mut h1 = ctl_h1(
                server,
                &ev,
                async move |_| {
                    calls2.set(calls2.get() + 1);
                    Ok(Response::Ok().build())
                },
                move |msg| match msg {
                    Control::Upgrade(mut upg) => {
                        assert_eq!(upg.get_ref().path(), "/ws");
                        assert_eq!(upg.get_mut().path(), "/ws");
                        assert!(!upg.io().is_closed());
                        if with_err {
                            Ok(upg.fail(io::Error::other("upgrade")))
                        } else {
                            Ok(upg.fail_with(Response::Forbidden().build()))
                        }
                    }
                    msg => Ok(msg.ack()),
                },
            );

            client.write(
                "GET /ws HTTP/1.1\r\nhost: a\r\nconnection: upgrade\r\n\
                 upgrade: websocket\r\n\r\nGET /next HTTP/1.1\r\nhost: a\r\n\r\n",
            );
            let res = timeout(Millis(1000), poll_fn(|cx| Pin::new(&mut h1).poll(cx))).await;
            assert!(matches!(res, Ok(Ok(()))));

            let buf = client.read_any();
            let status: &[u8] = if with_err {
                b"HTTP/1.1 500 Internal Server Error\r\n"
            } else {
                b"HTTP/1.1 403 Forbidden\r\n"
            };
            assert!(buf.starts_with(status), "{buf:?}");
            // the pipelined request is not processed
            assert_eq!(
                buf.windows(9).filter(|w| w == b"HTTP/1.1 ").count(),
                1,
                "{buf:?}"
            );
            assert_eq!(calls.get(), 0);
            assert_eq!(events(&ev), ["request", "upgrade", "service:UpgradeFailed"]);
        }
    }

    /// The control service can replace the response for a service error.
    #[crate::rt_test]
    async fn test_service_error_response_replaced() {
        for with_err in [false, true] {
            let (client, server) = IoTest::create();
            client.remote_buffer_cap(4096);
            let ev = Events::default();
            let mut h1 = ctl_h1(
                server,
                &ev,
                async |_| Err(io::Error::other("service")),
                move |msg| match msg {
                    Control::Disconnect(Reason::Error(mut err)) => {
                        assert_eq!(err.get_ref().to_string(), "service");
                        assert_eq!(err.get_mut().kind(), io::ErrorKind::Other);
                        if with_err {
                            Ok(err.fail(ProtocolError::Decode(
                                crate::http::error::DecodeError::Method,
                            )))
                        } else {
                            Ok(err.fail_with(Response::new(StatusCode::IM_A_TEAPOT)))
                        }
                    }
                    msg => Ok(msg.ack()),
                },
            );

            client.write("GET / HTTP/1.1\r\nhost: a\r\n\r\n");
            let res = timeout(Millis(1000), poll_fn(|cx| Pin::new(&mut h1).poll(cx))).await;
            assert!(matches!(res, Ok(Ok(()))));

            let buf = client.read_any();
            let status: &[u8] = if with_err {
                b"HTTP/1.1 400 Bad Request\r\n"
            } else {
                b"HTTP/1.1 418 I'm a teapot\r\n"
            };
            assert!(buf.starts_with(status), "{buf:?}");
            assert_eq!(events(&ev), ["request", "error"]);
        }
    }

    /// The control service can replace the response for a protocol error.
    #[crate::rt_test]
    async fn test_protocol_error_response_replaced() {
        for with_err in [false, true] {
            let (client, server) = IoTest::create();
            client.remote_buffer_cap(4096);
            let ev = Events::default();
            let mut h1 = ctl_h1(
                server,
                &ev,
                async |_| Ok(Response::Ok().build()),
                move |msg| match msg {
                    Control::Disconnect(Reason::ProtocolError(err)) => {
                        if with_err {
                            Ok(err.fail(io::Error::other("proto")))
                        } else {
                            Ok(err.fail_with(Response::new(StatusCode::IM_A_TEAPOT)))
                        }
                    }
                    msg => Ok(msg.ack()),
                },
            );

            client.write("GET / HTTP/1.1\r\nhost: a\r\ncontent-length: x\r\n\r\n");
            let res = timeout(Millis(1000), poll_fn(|cx| Pin::new(&mut h1).poll(cx))).await;
            assert!(matches!(res, Ok(Ok(()))));

            let buf = client.read_any();
            let status: &[u8] = if with_err {
                b"HTTP/1.1 500 Internal Server Error\r\n"
            } else {
                b"HTTP/1.1 418 I'm a teapot\r\n"
            };
            assert!(buf.starts_with(status), "{buf:?}");
            assert_eq!(events(&ev), ["decode"]);
        }
    }

    /// A rejected expectation sends the response, skips the service and
    /// closes the connection.
    #[crate::rt_test]
    async fn test_expect_failed() {
        for with_err in [false, true] {
            let (client, server) = IoTest::create();
            client.remote_buffer_cap(4096);
            let ev = Events::default();
            let calls = Rc::new(Cell::new(0));
            let calls2 = calls.clone();
            let mut h1 = ctl_h1(
                server,
                &ev,
                async move |_| {
                    calls2.set(calls2.get() + 1);
                    Ok(Response::Ok().build())
                },
                move |msg| match msg {
                    Control::Expect(mut exp) => {
                        assert_eq!(exp.get_mut().path(), "/upload");
                        if with_err {
                            Ok(exp.fail(io::Error::other("expect")))
                        } else {
                            Ok(exp.fail_with(Response::ExpectationFailed().build()))
                        }
                    }
                    msg => Ok(msg.ack()),
                },
            );

            client.write(
                "POST /upload HTTP/1.1\r\nhost: a\r\ncontent-length: 4\r\n\
                 expect: 100-continue\r\n\r\n",
            );
            let res = timeout(Millis(1000), poll_fn(|cx| Pin::new(&mut h1).poll(cx))).await;
            assert!(matches!(res, Ok(Ok(()))));

            let buf = client.read_any();
            let status: &[u8] = if with_err {
                b"HTTP/1.1 500 Internal Server Error\r\n"
            } else {
                b"HTTP/1.1 417 Expectation Failed\r\n"
            };
            assert!(buf.starts_with(status), "{buf:?}");
            assert!(!buf.windows(3).any(|w| w == b"100"), "{buf:?}");
            assert_eq!(calls.get(), 0);
            assert_eq!(events(&ev), ["request", "expect", "service:ExpectFailed"]);
        }
    }

    /// A response body error is reported as a protocol error and the
    /// connection is closed without completing the response.
    #[crate::rt_test]
    async fn test_response_body_error() {
        use futures_util::stream;

        let (client, server) = IoTest::create();
        client.remote_buffer_cap(4096);
        let ev = Events::default();
        let mut h1 = ctl_h1(
            server,
            &ev,
            async |_| {
                Ok(Response::Ok().streaming(stream::iter([
                    Ok(Bytes::from_static(b"part")),
                    Err(io::Error::other("body")),
                ])))
            },
            |msg| Ok(msg.ack()),
        );

        client.write("GET / HTTP/1.1\r\nhost: a\r\n\r\nGET /next HTTP/1.1\r\nhost: a\r\n\r\n");
        let res = timeout(Millis(1000), poll_fn(|cx| Pin::new(&mut h1).poll(cx))).await;
        assert!(matches!(res, Ok(Ok(()))));

        let buf = client.read_any();
        assert!(buf.starts_with(b"HTTP/1.1 200 OK\r\n"), "{buf:?}");
        assert!(buf.windows(4).any(|w| w == b"part"), "{buf:?}");
        // the chunked body is not terminated and no second response follows
        assert!(!buf.ends_with(b"0\r\n\r\n"), "{buf:?}");
        assert_eq!(
            buf.windows(9).filter(|w| w == b"HTTP/1.1 ").count(),
            1,
            "{buf:?}"
        );
        assert_eq!(events(&ev), ["request", "response-payload"]);
    }

    /// A transport read error is reported with the error.
    #[crate::rt_test]
    async fn test_peer_gone_with_error() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(4096);
        let ev = Events::default();
        let taken = Rc::new(Cell::new(false));
        let taken2 = taken.clone();
        let mut h1 = ctl_h1(
            server,
            &ev,
            async |_| Ok(Response::Ok().build()),
            move |msg| match msg {
                Control::Disconnect(Reason::PeerGone(mut p)) => {
                    assert_eq!(p.get_mut().unwrap().kind(), io::ErrorKind::ConnectionReset);
                    taken2.set(p.take().is_some());
                    assert!(p.get_ref().is_none());
                    Ok(p.ack())
                }
                msg => Ok(msg.ack()),
            },
        );

        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        client.read_error(io::Error::from(io::ErrorKind::ConnectionReset));
        let res = timeout(Millis(1000), poll_fn(|cx| Pin::new(&mut h1).poll(cx))).await;
        assert!(matches!(res, Ok(Ok(()))));
        assert!(taken.get());
        assert_eq!(events(&ev), ["peer-gone:true"]);
    }

    #[derive(Default)]
    struct GateDir {
        blocked: Cell<bool>,
        waker: RefCell<Option<std::task::Waker>>,
    }

    impl GateDir {
        fn poll(&self, res: Poll<nio::Readiness>, cx: &Context<'_>) -> Poll<nio::Readiness> {
            match res {
                Poll::Ready(nio::Readiness::Ready) if self.blocked.get() => {
                    *self.waker.borrow_mut() = Some(cx.waker().clone());
                    Poll::Pending
                }
                res => res,
            }
        }

        fn set(&self, blocked: bool) {
            self.blocked.set(blocked);
            if !blocked && let Some(waker) = self.waker.borrow_mut().take() {
                waker.wake();
            }
        }
    }

    /// Gate control, a blocked filter chain is not ready for reads or writes.
    #[derive(Clone, Default)]
    struct GateCtl(Rc<(GateDir, GateDir)>);

    impl GateCtl {
        fn block_read(&self, blocked: bool) {
            self.0.0.set(blocked);
        }

        fn block_write(&self, blocked: bool) {
            self.0.1.set(blocked);
        }
    }

    struct GateFilter<F>(F, GateCtl);

    impl<F: Filter> Filter for GateFilter<F> {
        fn query(&self, id: std::any::TypeId) -> Option<Box<dyn std::any::Any>> {
            self.0.query(id)
        }

        fn process_read_buf(&self, ctx: &mut nio::FilterCtx<'_>) -> io::Result<()> {
            self.0.process_read_buf(ctx)
        }

        fn process_write_buf(&self, ctx: &mut nio::FilterCtx<'_>) -> io::Result<()> {
            self.0.process_write_buf(ctx)
        }

        fn shutdown(&self, ctx: &mut nio::FilterCtx<'_>) -> io::Result<Poll<()>> {
            self.0.shutdown(ctx)
        }

        fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<nio::Readiness> {
            self.1.0.0.poll(self.0.poll_read_ready(cx), cx)
        }

        fn poll_write_ready(&self, cx: &mut Context<'_>) -> Poll<nio::Readiness> {
            self.1.0.1.poll(self.0.poll_write_ready(cx), cx)
        }
    }

    type GatedH1 = Dispatcher<GateFilter<Base>, body::Body, io::Error>;

    /// Dispatcher over a gated filter chain, the service reads the request
    /// payload and responds with a body of `size` bytes.
    fn gated_h1(server: IoTest, cfg: HttpServiceConfig, size: usize) -> (GateCtl, GatedH1) {
        let gate = GateCtl::default();
        let g = gate.clone();
        let config: SharedCfg = SharedCfg::new("SVC").add(cfg).into();
        let io = nio::Io::new(server, config).map_filter(move |f| GateFilter(f, g));
        let h1 = Dispatcher::new(
            0,
            io,
            Pipeline::new(
                (),
                fn_service(move |mut req: Request| async move {
                    let mut pl = req.take_payload();
                    while let Some(item) = pl.recv().await {
                        item.map_err(io::Error::other)?;
                    }
                    Ok::<_, io::Error>(Response::Ok().body("x".repeat(size)))
                }),
            ),
            None,
            DispatcherConfig::default(),
        );
        (gate, h1)
    }

    fn spawn_gated_h1(
        server: IoTest,
        cfg: HttpServiceConfig,
        size: usize,
    ) -> (GateCtl, nio::IoRef) {
        let (gate, h1) = gated_h1(server, cfg, size);
        let io = h1.inner.io.get_ref();
        crate::rt::spawn(async move {
            let _ = h1.await;
        });
        (gate, io)
    }

    /// Reads a response with a body of `size` bytes.
    async fn read_response(client: &IoTest, size: usize) -> Bytes {
        let mut buf = BytesMut::new();
        loop {
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n")
                && buf.len() - pos - 4 >= size
            {
                return buf.freeze();
            }
            buf.extend_from_slice(&client.read().await.unwrap());
        }
    }

    /// The keep-alive timer does not run while the filter chain pauses
    /// reading.
    #[crate::rt_test]
    async fn test_filter_pause_suspends_keepalive() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        let (gate, io) = spawn_gated_h1(
            server,
            HttpServiceConfig::new()
                .set_client_timeout(Seconds::ZERO)
                .set_keepalive(Seconds(1)),
            1,
        );

        client.write("GET / HTTP/1.1\r\nhost: localhost\r\n\r\n");
        assert!(
            read_response(&client, 1)
                .await
                .starts_with(b"HTTP/1.1 200 OK\r\n")
        );

        // the peer sends a request while the filter is not ready
        gate.block_read(true);
        client.write("GET / HTTP/1.1\r\nhost: localhost\r\n\r\n");
        sleep(Millis(100)).await;
        assert!(io.is_read_filter_paused());

        sleep(Millis(2000)).await;
        assert!(io.is_active());

        gate.block_read(false);
        assert!(
            read_response(&client, 1)
                .await
                .starts_with(b"HTTP/1.1 200 OK\r\n")
        );

        // keep-alive is armed again once reading resumes
        sleep(Millis(2500)).await;
        assert!(client.is_closed());
    }

    /// The request-head read timer does not run while the filter chain
    /// pauses reading.
    #[crate::rt_test]
    async fn test_filter_pause_suspends_headers_timer() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        let (gate, io) = spawn_gated_h1(
            server,
            HttpServiceConfig::new()
                .set_headers_read_rate(Seconds(1), Seconds(2), 1024)
                .set_keepalive(KeepAlive::Disabled),
            1,
        );

        client.write("GET / HTTP/1.1\r\n");
        sleep(Millis(100)).await;

        gate.block_read(true);
        client.write("host: localhost\r\n");
        sleep(Millis(100)).await;
        assert!(io.is_read_filter_paused());

        sleep(Millis(3000)).await;
        assert!(io.is_active());

        // the timer resumes once reading resumes
        gate.block_read(false);
        sleep(Millis(100)).await;
        assert!(!io.is_read_filter_paused());
        assert!(io.is_active());
        sleep(Millis(2000)).await;
        assert!(!io.is_active());
        assert!(client.read_any().starts_with(b"HTTP/1.1 408"));
    }

    /// The payload read timer does not run while the filter chain pauses
    /// reading.
    #[crate::rt_test]
    async fn test_filter_pause_suspends_payload_timer() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        let (gate, io) = spawn_gated_h1(
            server,
            HttpServiceConfig::new()
                .set_payload_read_rate(Seconds(1), Seconds(2), 1024)
                .set_keepalive(KeepAlive::Disabled),
            1,
        );

        client.write("POST / HTTP/1.1\r\nhost: localhost\r\ncontent-length: 4\r\n\r\nab");
        sleep(Millis(100)).await;

        gate.block_read(true);
        client.write("cd");
        sleep(Millis(100)).await;
        assert!(io.is_read_filter_paused());

        sleep(Millis(3000)).await;
        assert!(io.is_active());

        gate.block_read(false);
        assert!(
            read_response(&client, 1)
                .await
                .starts_with(b"HTTP/1.1 200 OK\r\n")
        );
    }

    /// The write timer does not run while the filter chain pauses writing.
    #[crate::rt_test]
    async fn test_filter_pause_suspends_write_timeout() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1_048_576);
        let size = 128 * 1024;
        let (gate, io) = spawn_gated_h1(
            server,
            HttpServiceConfig::new().set_write_timeout(Seconds(1)),
            size,
        );

        // the response waits for the filter, write backpressure is enabled
        gate.block_write(true);
        client.write("GET / HTTP/1.1\r\nhost: localhost\r\n\r\n");
        sleep(Millis(100)).await;
        assert!(io.is_write_filter_paused());
        assert!(io.is_wr_backpressure());

        sleep(Millis(2500)).await;
        assert!(io.is_active());

        gate.block_write(false);
        let buf = read_response(&client, size).await;
        assert!(buf.starts_with(b"HTTP/1.1 200 OK\r\n"));
        assert!(!io.is_write_filter_paused());
    }

    /// The write timer starts over once the filter chain resumes writing.
    #[crate::rt_test]
    async fn test_write_timeout_restarts_after_filter_pause() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(0);
        let (gate, io) = spawn_gated_h1(
            server,
            HttpServiceConfig::new().set_write_timeout(Seconds(1)),
            128 * 1024,
        );

        gate.block_write(true);
        client.write("GET / HTTP/1.1\r\nhost: localhost\r\n\r\n");
        sleep(Millis(1500)).await;
        assert!(io.is_write_filter_paused());
        assert!(io.is_active());

        // the peer does not read, the timeout starts over once writes resume
        gate.block_write(false);
        sleep(Millis(500)).await;
        assert!(!io.is_write_filter_paused());
        assert!(io.is_active());
        sleep(Millis(2000)).await;
        assert!(client.is_closed());
    }

    /// A write timer expiry before the dispatcher notices a filter write
    /// pause suspends the timer.
    #[crate::rt_test]
    async fn test_write_timer_expiry_during_filter_pause() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(0);
        let (gate, mut h1) = gated_h1(
            server,
            HttpServiceConfig::new()
                .set_write_timeout(Seconds(1))
                .set_client_timeout(Seconds(5)),
            128 * 1024,
        );

        client.write("GET / HTTP/1.1\r\nhost: localhost\r\n\r\n");
        sleep(Millis(50)).await;
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        assert!(h1.inner.io.is_wr_backpressure());
        assert_eq!(h1.inner.timers.active, Timer::Write);

        // a read event turns the io task, it notices the pause. The "G" stays
        // as a partial next request, the client timeout must outlast the checks
        gate.block_write(true);
        client.write("G");
        sleep(Millis(50)).await;
        assert!(h1.inner.io.is_write_filter_paused());

        assert!(h1.inner.write_timer_expired().is_ok());
        assert_eq!(h1.inner.timers.active, Timer::Write);
        assert!(h1.inner.timers.write_suspended);
        assert!(!h1.inner.io.timer_handle().is_set());

        gate.block_write(false);
        client.remote_buffer_cap(1_048_576);
        let res = timeout(Millis(1000), poll_fn(|cx| Pin::new(&mut h1).poll(cx))).await;
        assert!(res.is_err());
        assert!(
            read_response(&client, 128 * 1024)
                .await
                .starts_with(b"HTTP/1.1 200 OK\r\n")
        );
    }
}
