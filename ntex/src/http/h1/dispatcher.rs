//! HTTP/1 protocol dispatcher
use std::task::{ready, Context, Poll};
use std::{error, future, io, marker, mem, pin::Pin, rc::Rc};

use crate::channel::bstream;
use crate::io::{Decoded, Filter, Io, IoStatusUpdate, RecvError};
use crate::service::{PipelineCall, Service};
use crate::time::Seconds;
use crate::util::Either;

use crate::http::body::{BodySize, MessageBody, ResponseBody};
use crate::http::error::{PayloadError, ResponseError};
use crate::http::message::CurrentIo;
use crate::http::{self, config::DispatcherConfig, request::Request, response::Response};

use super::control::{Control, ControlAck, ControlFlags, ControlResult};
use super::decoder::{PayloadDecoder, PayloadItem, PayloadType};
use super::{codec::Codec, Message, ProtocolError};

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
    pub struct Flags: u8 {
        /// Stopping
        const SENDPAYLOAD_AND_STOP = 0b0000_0010;
        /// Complete operation and disconnect
        const DISCONNECT           = 0b0000_0100;
        /// Keep-alive is enabled
        const READ_KA_TIMEOUT      = 0b0001_0000;
        /// Read headers timer is enabled
        const READ_HDRS_TIMEOUT    = 0b0010_0000;
        /// Read headers payload is enabled
        const READ_PL_TIMEOUT      = 0b0100_0000;
    }
}

pin_project_lite::pin_project! {
    /// Dispatcher for HTTP/1.1 protocol
    pub struct Dispatcher<F, S: Service<Request>, B, C: Service<Control<F, S::Error>>>
    where
        F: 'static,
        S::Error: 'static,
    {
        st: State<F, C, S, B>,
        inner: DispatcherInner<F, C, S, B>,
    }
}

#[derive(Debug)]
enum State<F, C, S, B>
where
    F: 'static,
    S: Service<Request>,
    S::Error: 'static,
    C: Service<Control<F, S::Error>>,
{
    CallPublish {
        fut: PipelineCall<S, Request>,
    },
    CallControl {
        fut: PipelineCall<C, Control<F, S::Error>>,
    },
    ReadRequest,
    ReadPayload,
    SendPayload {
        body: ResponseBody<B>,
    },
    Stop {
        fut: Option<PipelineCall<C, Control<F, S::Error>>>,
    },
}

struct DispatcherInner<F, C, S, B> {
    io: Rc<Io<F>>,
    flags: Flags,
    codec: Codec,
    config: Rc<DispatcherConfig<S, C>>,
    payload: Option<(PayloadDecoder, bstream::Sender<PayloadError>)>,
    read_remains: u32,
    read_consumed: u32,
    read_max_timeout: Seconds,
    _t: marker::PhantomData<(S, B)>,
}

impl<F, S, B, C> Dispatcher<F, S, B, C>
where
    F: Filter,
    C: Service<Control<F, S::Error>, Response = ControlAck>,
    S: Service<Request>,
    S::Error: ResponseError,
    S::Response: Into<Response<B>>,
    B: MessageBody,
{
    /// Construct new `Dispatcher` instance with outgoing messages stream.
    pub(in crate::http) fn new(io: Io<F>, config: Rc<DispatcherConfig<S, C>>) -> Self {
        let codec = Codec::new(config.timer.clone(), config.keep_alive_enabled());
        io.set_disconnect_timeout(config.client_disconnect);

        // slow-request timer
        let (flags, max_timeout) = if let Some(cfg) = config.headers_read_rate() {
            io.start_timer(cfg.timeout);
            (Flags::READ_HDRS_TIMEOUT, cfg.max_timeout)
        } else {
            (Flags::empty(), Seconds::ZERO)
        };

        Dispatcher {
            st: State::ReadRequest,
            inner: DispatcherInner {
                flags,
                codec,
                config,
                io: Rc::new(io),
                payload: None,
                read_remains: 0,
                read_consumed: 0,
                read_max_timeout: max_timeout,
                _t: marker::PhantomData,
            },
        }
    }
}

impl<F, S, B, C> future::Future for Dispatcher<F, S, B, C>
where
    F: Filter,
    C: Service<Control<F, S::Error>, Response = ControlAck> + 'static,
    C::Error: error::Error,
    S: Service<Request> + 'static,
    S::Error: ResponseError + 'static,
    S::Response: Into<Response<B>>,
    B: MessageBody,
{
    type Output = Result<(), Box<dyn error::Error>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();
        let inner = &mut this.inner;

        loop {
            *this.st = match this.st {
                // handle publish service responses
                State::CallPublish { fut } => match Pin::new(fut).poll(cx) {
                    Poll::Ready(Ok(res)) => {
                        let (res, body) = res.into().into_parts();
                        inner.send_response(res, body)
                    }
                    Poll::Ready(Err(err)) => inner.control(Control::err(err)),
                    Poll::Pending => {
                        // state changed because of error.
                        // spawn current publish future to runtime
                        // so it could complete error handling
                        let st = ready!(inner.poll_request(cx));
                        if inner.payload.is_some() {
                            if let State::CallPublish { fut } =
                                mem::replace(&mut *this.st, State::ReadRequest)
                            {
                                crate::rt::spawn(fut);
                            }
                        }
                        st
                    }
                },
                // handle control service responses
                State::CallControl { fut } => match Pin::new(fut).poll(cx) {
                    Poll::Ready(Ok(ControlAck { result, flags })) => {
                        if flags.contains(ControlFlags::CONTINUE) {
                            let result = inner.io.with_write_buf(|buf| {
                                buf.extend_from_slice(b"HTTP/1.1 100 Continue\r\n\r\n")
                            });
                            if let Err(err) = result {
                                *this.st = inner.ctl_peer_gone(Some(err));
                                continue;
                            }
                        }
                        if flags.contains(ControlFlags::DISCONNECT) {
                            inner.flags.insert(Flags::DISCONNECT);
                        }

                        match result {
                            ControlResult::Publish(req) => inner.publish(req),
                            ControlResult::Response(res, body) => {
                                inner.send_response(res, body.into())
                            }
                            ControlResult::Expect(req) => {
                                inner.control(Control::expect(req))
                            }
                            ControlResult::Upgrade(req) => inner.ctl_upgrade(req),
                            ControlResult::Stop => inner.stop(),
                        }
                    }
                    Poll::Ready(Err(err)) => {
                        log::error!("{}: Control plain error: {}", inner.io.tag(), err);
                        return Poll::Ready(Err(Box::new(err)));
                    }
                    Poll::Pending => ready!(inner.poll_request(cx)),
                },
                // read request and call service
                State::ReadRequest => {
                    if inner.flags.contains(Flags::SENDPAYLOAD_AND_STOP) {
                        inner.stop()
                    } else {
                        ready!(inner.poll_read_request(cx))
                    }
                }
                // consume request's payload
                State::ReadPayload => {
                    let result = inner.poll_request_payload(cx);
                    if inner.flags.contains(Flags::SENDPAYLOAD_AND_STOP) {
                        inner.stop()
                    } else {
                        ready!(result).unwrap_or(State::ReadRequest)
                    }
                }
                // send response body
                State::SendPayload { body } => {
                    ready!(inner.poll_send_payload(cx, body))
                }
                // shutdown io
                State::Stop { fut } => {
                    if let Some(ref mut f) = fut {
                        let _ = ready!(Pin::new(f).poll(cx));
                        fut.take();
                    }
                    log::debug!("{}: Dispatcher is stopped", inner.io.tag());

                    inner.io.stop_timer();
                    return Poll::Ready(
                        ready!(inner.io.poll_shutdown(cx)).map_err(From::from),
                    );
                }
            }
        }
    }
}

impl<F, C, S, B> DispatcherInner<F, C, S, B>
where
    F: Filter,
    C: Service<Control<F, S::Error>, Response = ControlAck> + 'static,
    S: Service<Request> + 'static,
    S::Error: ResponseError,
    S::Response: Into<Response<B>>,
    B: MessageBody,
{
    fn poll_read_request(&mut self, cx: &mut Context<'_>) -> Poll<State<F, C, S, B>> {
        // stop dispatcher
        if self.config.is_shutdown() {
            log::trace!("{}: Service is shutting down", self.io.tag());
            return Poll::Ready(self.stop());
        }

        log::trace!("{}: Trying to read http message", self.io.tag());

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
                    PayloadType::Payload(decoder) => {
                        let (ps, pl) = bstream::channel();
                        req.replace_payload(http::Payload::H1(pl));
                        self.payload = Some((decoder, ps));
                    }
                    PayloadType::Stream(decoder) => {
                        let (ps, pl) = bstream::channel();
                        req.replace_payload(http::Payload::H1(pl));
                        self.payload = Some((decoder, ps));
                    }
                };
                self.control(Control::new_req(req))
            }
            Err(RecvError::WriteBackpressure) => {
                if let Err(err) = ready!(self.io.poll_flush(cx, false)) {
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
                if self.flags.contains(Flags::READ_HDRS_TIMEOUT) {
                    if let Err(err) = self.handle_timeout() {
                        log::trace!("{}: Slow request timeout", self.io.tag());
                        self.ctl_proto_err(err)
                    } else {
                        ready!(self.poll_read_request(cx))
                    }
                } else {
                    log::trace!("{}: Keep-alive timeout, close connection", self.io.tag());
                    self.stop()
                }
            }
            Err(RecvError::Stop) => {
                log::trace!("{}: Dispatcher is instructed to stop", self.io.tag());
                self.stop()
            }
        };

        Poll::Ready(st)
    }

    fn send_response(
        &mut self,
        msg: Response<()>,
        body: ResponseBody<B>,
    ) -> State<F, C, S, B> {
        log::trace!(
            "{}: Sending response: {:?} body: {:?}",
            self.io.tag(),
            msg,
            body.size()
        );

        // we dont need to process responses if socket is disconnected
        // but we still want to handle requests with app service
        // so we skip response processing for droppped connection
        if self.io.is_closed() {
            self.stop()
        } else {
            let result = self
                .io
                .encode(Message::Item((msg, body.size())), &self.codec)
                .map_err(|err| {
                    if let Some(ref mut payload) = self.payload {
                        payload.1.set_error(PayloadError::Incomplete(None));
                    }
                    err
                });

            match result {
                Ok(()) => match body.size() {
                    BodySize::None | BodySize::Empty => {
                        if self
                            .flags
                            .intersects(Flags::DISCONNECT | Flags::SENDPAYLOAD_AND_STOP)
                        {
                            self.stop()
                        } else if self.payload.is_some() {
                            State::ReadPayload
                        } else {
                            State::ReadRequest
                        }
                    }
                    _ => State::SendPayload { body },
                },
                Err(_) if self.flags.contains(Flags::DISCONNECT) => self.stop(),
                Err(err) => self.ctl_proto_err(err.into()),
            }
        }
    }

    fn poll_send_payload(
        &mut self,
        cx: &mut Context<'_>,
        body: &mut ResponseBody<B>,
    ) -> Poll<State<F, C, S, B>> {
        if self.io.is_closed() {
            return Poll::Ready(self.stop());
        } else if !self.flags.contains(Flags::SENDPAYLOAD_AND_STOP) {
            if let Poll::Ready(Some(_)) = self.poll_request_payload(cx) {
                self.flags.insert(Flags::SENDPAYLOAD_AND_STOP);
            }
        }
        loop {
            let _ = ready!(self.io.poll_flush(cx, false));
            let item = ready!(body.poll_next_chunk(cx));

            let st = match item {
                Some(Ok(item)) => {
                    log::trace!("{}: Got response chunk: {:?}", self.io.tag(), item.len());
                    match self.io.encode(Message::Chunk(Some(item)), &self.codec) {
                        Ok(_) => continue,
                        Err(err) => self.ctl_proto_err(err.into()),
                    }
                }
                None => {
                    log::trace!("{}: Response payload eof {:?}", self.io.tag(), self.flags);
                    if let Err(err) = self.io.encode(Message::Chunk(None), &self.codec) {
                        self.ctl_proto_err(err.into())
                    } else if self.flags.contains(Flags::DISCONNECT) {
                        self.stop()
                    } else if self.payload.is_some() {
                        State::ReadPayload
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
    fn poll_request(&mut self, cx: &mut Context<'_>) -> Poll<State<F, C, S, B>> {
        if self.payload.is_some() {
            if let Some(st) = ready!(self.poll_request_payload(cx)) {
                Poll::Ready(st)
            } else {
                Poll::Pending
            }
        } else {
            // check for io changes, could close while waiting for service call
            match ready!(self.io.poll_status_update(cx)) {
                IoStatusUpdate::KeepAlive => Poll::Pending,
                IoStatusUpdate::Stop | IoStatusUpdate::PeerGone(_) => {
                    Poll::Ready(self.stop())
                }
                IoStatusUpdate::WriteBackpressure => Poll::Pending,
            }
        }
    }

    fn set_payload_error(&mut self, err: PayloadError) {
        if let Some(ref mut payload) = self.payload {
            payload.1.set_error(err);
        }
    }

    /// Process request's payload
    fn poll_request_payload(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<State<F, C, S, B>>> {
        if let Err(err) = ready!(self._poll_request_payload::<F>(None, cx)) {
            Poll::Ready(Some(match err {
                Either::Left(e) => self.ctl_proto_err(e),
                Either::Right(e) => self.ctl_peer_gone(e),
            }))
        } else {
            Poll::Ready(None)
        }
    }

    /// Process request's payload
    fn _poll_request_payload<Fi>(
        &mut self,
        io: Option<&Io<Fi>>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), Either<ProtocolError, Option<io::Error>>>> {
        // check if payload data is required
        if self.payload.is_none() {
            return Poll::Ready(Ok(()));
        };

        match self.payload.as_ref().unwrap().1.poll_ready(cx) {
            Poll::Ready(bstream::Status::Ready) => {
                // read request payload
                let mut updated = false;
                loop {
                    let recv_result = io
                        .map(|io| {
                            io.poll_recv_decode(&self.payload.as_ref().unwrap().0, cx)
                        })
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
                            self.flags.remove(Flags::READ_PL_TIMEOUT);
                            self.payload.as_mut().unwrap().1.feed_eof();
                            self.payload = None;
                            break;
                        }
                        Err(err) => {
                            let err = match err {
                                RecvError::WriteBackpressure => {
                                    let flush_result = io
                                        .map(|io| io.poll_flush(cx, false))
                                        .unwrap_or_else(|| self.io.poll_flush(cx, false));

                                    if flush_result
                                        .map_err(|e| Either::Right(Some(e)))?
                                        .is_pending()
                                    {
                                        break;
                                    } else {
                                        continue;
                                    }
                                }
                                RecvError::KeepAlive => {
                                    if let Err(err) = self.handle_timeout() {
                                        Either::Left(err)
                                    } else {
                                        continue;
                                    }
                                }
                                RecvError::Stop => {
                                    self.set_payload_error(PayloadError::EncodingCorrupted);
                                    Either::Right(Some(io::Error::new(
                                        io::ErrorKind::Other,
                                        "Dispatcher stopped",
                                    )))
                                }
                                RecvError::PeerGone(err) => {
                                    self.set_payload_error(PayloadError::EncodingCorrupted);
                                    Either::Right(err)
                                }
                                RecvError::Decoder(e) => {
                                    self.set_payload_error(PayloadError::EncodingCorrupted);
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
                // stop payload timer
                if self.flags.contains(Flags::READ_PL_TIMEOUT) {
                    self.flags.remove(Flags::READ_PL_TIMEOUT);
                    self.io.stop_timer();
                }
                Poll::Pending
            }
            Poll::Ready(bstream::Status::Dropped) => {
                // service call is not interested in payload
                // wait until future completes and then close
                // connection
                self.payload = None;
                self.flags.insert(Flags::SENDPAYLOAD_AND_STOP);
                Poll::Pending
            }
        }
    }

    fn handle_timeout(&mut self) -> Result<(), ProtocolError> {
        // check read rate
        let cfg = if self.flags.contains(Flags::READ_HDRS_TIMEOUT) {
            &self.config.headers_read_rate
        } else if self.flags.contains(Flags::READ_PL_TIMEOUT) {
            &self.config.payload_read_rate
        } else {
            return Ok(());
        };

        if let Some(ref cfg) = cfg {
            let total = if self.flags.contains(Flags::READ_HDRS_TIMEOUT) {
                let total = (self.read_remains - self.read_consumed)
                    .try_into()
                    .unwrap_or(u16::MAX);
                self.read_remains = 0;
                total
            } else {
                let total = (self.read_remains + self.read_consumed)
                    .try_into()
                    .unwrap_or(u16::MAX);
                self.read_consumed = 0;
                total
            };

            if total > cfg.rate {
                // update max timeout
                if !cfg.max_timeout.is_zero() {
                    self.read_max_timeout =
                        Seconds(self.read_max_timeout.0.saturating_sub(cfg.timeout.0));
                }

                // start timer for next period
                if cfg.max_timeout.is_zero() || !self.read_max_timeout.is_zero() {
                    log::trace!(
                        "{}: Bytes read rate {:?}, extend timer",
                        self.io.tag(),
                        total
                    );
                    self.io.start_timer(cfg.timeout);
                    return Ok(());
                }
            }

            log::trace!(
                "{}: Timeout during reading, {:?}",
                self.io.tag(),
                self.flags
            );
            if self.flags.contains(Flags::READ_PL_TIMEOUT) {
                self.set_payload_error(PayloadError::Io(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Keep-alive",
                )));
                Err(ProtocolError::SlowPayloadTimeout)
            } else {
                Err(ProtocolError::SlowRequestTimeout)
            }
        } else {
            Ok(())
        }
    }

    fn update_hdrs_timer(
        &mut self,
        decoded: &Decoded<(Request, PayloadType)>,
    ) -> Option<State<F, C, S, B>> {
        // got parsed frame
        if decoded.item.is_some() {
            self.read_remains = 0;
            self.flags.remove(
                Flags::READ_KA_TIMEOUT | Flags::READ_HDRS_TIMEOUT | Flags::READ_PL_TIMEOUT,
            );
        } else if self.flags.contains(Flags::READ_HDRS_TIMEOUT) {
            // received new data but not enough for parsing complete frame
            self.read_remains = decoded.remains as u32;
        } else if self.read_remains == 0 && decoded.remains == 0 {
            // no new data, start keep-alive timer
            if self.codec.keepalive() {
                if !self.flags.contains(Flags::READ_KA_TIMEOUT)
                    && self.config.keep_alive_enabled()
                {
                    log::debug!(
                        "{}: Start keep-alive timer {:?}",
                        self.io.tag(),
                        self.config.keep_alive
                    );
                    self.flags.insert(Flags::READ_KA_TIMEOUT);
                    self.io.start_timer(self.config.keep_alive);
                }
            } else {
                self.io.close();
                return Some(self.stop());
            }
        } else if let Some(ref cfg) = self.config.headers_read_rate {
            log::debug!(
                "{}: Start headers read timer {:?}",
                self.io.tag(),
                cfg.timeout
            );

            // we got new data but not enough to parse single frame
            // start read timer
            self.flags
                .remove(Flags::READ_KA_TIMEOUT | Flags::READ_PL_TIMEOUT);
            self.flags.insert(Flags::READ_HDRS_TIMEOUT);

            self.read_consumed = 0;
            self.read_remains = decoded.remains as u32;
            self.read_max_timeout = cfg.max_timeout;
            self.io.start_timer(cfg.timeout);
        }
        None
    }

    fn update_payload_timer(&mut self, decoded: &Decoded<PayloadItem>) {
        if self.flags.contains(Flags::READ_PL_TIMEOUT) {
            self.read_remains = decoded.remains as u32;
            self.read_consumed += decoded.consumed as u32;
        } else if let Some(ref cfg) = self.config.payload_read_rate {
            log::debug!("{}: Start payload timer {:?}", self.io.tag(), cfg.timeout);

            // start payload timer
            self.flags.insert(Flags::READ_PL_TIMEOUT);

            self.read_remains = decoded.remains as u32;
            self.read_consumed = decoded.consumed as u32;
            self.read_max_timeout = cfg.max_timeout;
            self.io.start_timer(cfg.timeout);
        }
    }

    fn publish(&self, req: Request) -> State<F, C, S, B> {
        State::CallPublish {
            fut: self.config.service.call_nowait(req),
        }
    }

    fn control(&self, req: Control<F, S::Error>) -> State<F, C, S, B> {
        State::CallControl {
            fut: self.config.control.call_nowait(req),
        }
    }

    fn ctl_proto_err(&self, err: ProtocolError) -> State<F, C, S, B> {
        State::CallControl {
            fut: self.config.control.call_nowait(Control::proto_err(err)),
        }
    }

    fn ctl_peer_gone(&self, err: Option<io::Error>) -> State<F, C, S, B> {
        State::CallControl {
            fut: self.config.control.call_nowait(Control::peer_gone(err)),
        }
    }

    fn ctl_upgrade(&mut self, req: Request) -> State<F, C, S, B> {
        self.codec.reset_upgrade();
        let msg = Control::upgrade(req, self.io.clone(), self.codec.clone());
        self.control(msg)
    }

    fn stop(&mut self) -> State<F, C, S, B> {
        State::Stop {
            fut: Some(self.config.control.call_nowait(Control::closed())),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::{cell::Cell, future::poll_fn, future::Future, sync::Arc};

    use ntex_h2::Config;
    use rand::Rng;

    use super::*;
    use crate::http::config::ServiceConfig;
    use crate::http::h1::{ClientCodec, DefaultControlService};
    use crate::http::{body, ResponseHead, StatusCode};
    use crate::io::{self as nio, Base};
    use crate::service::{fn_service, IntoService};
    use crate::util::{lazy, stream_recv, Bytes, BytesMut};
    use crate::{codec::Decoder, testing::Io, time::sleep, time::Millis};

    const BUFFER_SIZE: usize = 32_768;

    /// Create http/1 dispatcher.
    pub(crate) fn h1<F, S, B>(
        stream: Io,
        service: F,
    ) -> Dispatcher<Base, S, B, DefaultControlService>
    where
        F: IntoService<S, Request>,
        S: Service<Request>,
        S::Error: ResponseError + 'static,
        S::Response: Into<Response<B>>,
        B: MessageBody,
    {
        let config = ServiceConfig::new(
            Seconds(5).into(),
            Seconds(1),
            Seconds::ZERO,
            Millis(5_000),
            Config::server(),
        );
        Dispatcher::new(
            nio::Io::new(stream),
            Rc::new(DispatcherConfig::new(
                config,
                service.into_service(),
                DefaultControlService,
            )),
        )
    }

    pub(crate) fn spawn_h1<F, S, B>(stream: Io, service: F)
    where
        F: IntoService<S, Request>,
        S: Service<Request> + 'static,
        S::Error: ResponseError,
        S::Response: Into<Response<B>>,
        B: MessageBody + 'static,
    {
        crate::rt::spawn(Dispatcher::<Base, S, B, _>::new(
            nio::Io::new(stream),
            Rc::new(DispatcherConfig::new(
                ServiceConfig::default(),
                service.into_service(),
                DefaultControlService,
            )),
        ));
    }

    fn load(decoder: &mut ClientCodec, buf: &mut BytesMut) -> ResponseHead {
        decoder.decode(buf).unwrap().unwrap()
    }

    #[crate::rt_test]
    async fn test_on_request() {
        let (client, server) = Io::create();
        client.remote_buffer_cap(1024);
        client.write("GET /test HTTP/1.0\r\n\r\n");

        let data = Rc::new(Cell::new(false));
        let data2 = data.clone();
        let config = ServiceConfig::new(
            Seconds(5).into(),
            Seconds(1),
            Seconds::ZERO,
            Millis(5_000),
            Config::server(),
        );
        let mut h1 = Dispatcher::<_, _, _, _>::new(
            nio::Io::new(server),
            Rc::new(DispatcherConfig::new(
                config,
                fn_service(|_| {
                    Box::pin(async { Ok::<_, io::Error>(Response::Ok().finish()) })
                }),
                fn_service(move |req: Control<_, _>| {
                    if let Control::NewRequest(_) = req {
                        data2.set(true);
                    }
                    async move { Ok::<_, std::convert::Infallible>(req.ack()) }
                }),
            )),
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
        let (client, server) = Io::create();
        client.remote_buffer_cap(1024);
        client.write("GET /test HTTP/1\r\n\r\n");

        let mut h1 = h1(server, |_| {
            Box::pin(async { Ok::<_, io::Error>(Response::Ok().finish()) })
        });
        sleep(Millis(50)).await;
        // required because io shutdown is async oper
        let _ = lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_ready();
        sleep(Millis(50)).await;

        assert!(poll_fn(|cx| Pin::new(&mut h1).poll(cx)).await.is_ok());
        assert!(h1.inner.io.is_closed());
        sleep(Millis(50)).await;

        client.local_buffer(|buf| assert_eq!(&buf[..26], b"HTTP/1.1 400 Bad Request\r\n"));

        client.close().await;
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_ready());
        assert!(h1.inner.io.is_closed());
    }

    #[crate::rt_test]
    async fn test_pipeline() {
        let (client, server) = Io::create();
        client.remote_buffer_cap(4096);
        let mut decoder = ClientCodec::default();
        spawn_h1(server, |_| async {
            Ok::<_, io::Error>(Response::Ok().finish())
        });

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
        let (client, server) = Io::create();
        client.remote_buffer_cap(4096);
        let mut decoder = ClientCodec::default();

        spawn_h1(server, |mut req: Request| async move {
            let mut p = req.take_payload();
            while (stream_recv(&mut p).await).is_some() {}
            Ok::<_, io::Error>(Response::Ok().finish())
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
        let (client, server) = Io::create();
        client.remote_buffer_cap(4096);
        let mut decoder = ClientCodec::default();
        spawn_h1(server, |_| async {
            sleep(Millis(100)).await;
            Ok::<_, io::Error>(Response::Ok().finish())
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
    /// h1 dispatcher still processes all incoming requests
    /// but it does not write any data to socket
    async fn test_write_disconnected() {
        let num = Arc::new(AtomicUsize::new(0));
        let num2 = num.clone();

        let (client, server) = Io::create();
        spawn_h1(server, move |_| {
            num2.fetch_add(1, Ordering::Relaxed);
            async { Ok::<_, io::Error>(Response::Ok().finish()) }
        });

        client.remote_buffer_cap(1024);
        client.write("GET /test HTTP/1.1\r\n\r\n");
        client.write("GET /test HTTP/1.1\r\n\r\n");
        client.write("GET /test HTTP/1.1\r\n\r\n");
        client.close().await;
        assert!(client.is_server_dropped());
        assert!(client.read_any().is_empty());

        // only first request get handled
        assert_eq!(num.load(Ordering::Relaxed), 1);
    }

    /// max http message size is 32k (no payload)
    #[crate::rt_test]
    async fn test_read_large_message() {
        let (client, server) = Io::create();
        client.remote_buffer_cap(4096);

        let mut h1 = h1(server, |_| {
            Box::pin(async { Ok::<_, io::Error>(Response::Ok().finish()) })
        });
        crate::util::PoolId::P0
            .set_read_params(15 * 1024, 1024)
            .set_write_params(15 * 1024, 1024);
        h1.inner
            .io
            .set_memory_pool(crate::util::PoolId::P0.pool_ref());

        let mut decoder = ClientCodec::default();

        // generate large http message
        let data = rand::thread_rng()
            .sample_iter(&rand::distributions::Alphanumeric)
            .take(70_000)
            .map(char::from)
            .collect::<String>();
        client.write("GET /test HTTP/1.1\r\nContent-Length: ");
        client.write(data);
        sleep(Millis(50)).await;

        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        sleep(Millis(50)).await;
        poll_fn(|cx| Pin::new(&mut h1).poll(cx)).await.unwrap();
        assert!(h1.inner.io.is_closed());

        let mut buf = BytesMut::from(&client.read().await.unwrap()[..]);
        assert_eq!(load(&mut decoder, &mut buf).status, StatusCode::BAD_REQUEST);
    }

    #[crate::rt_test]
    async fn test_read_backpressure() {
        let mark = Arc::new(AtomicBool::new(false));
        let mark2 = mark.clone();

        let (client, server) = Io::create();
        client.remote_buffer_cap(4096);
        spawn_h1(server, move |mut req: Request| {
            let m = mark2.clone();
            async move {
                // read one chunk
                let mut pl = req.take_payload();
                let _ = stream_recv(&mut pl).await.unwrap().unwrap();
                m.store(true, Ordering::Relaxed);
                // sleep
                sleep(Millis(999_999_000)).await;
                Ok::<_, io::Error>(Response::Ok().finish())
            }
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

    #[crate::rt_test]
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
            ) -> Poll<Option<Result<Bytes, Box<dyn error::Error>>>> {
                let data = rand::thread_rng()
                    .sample_iter(&rand::distributions::Alphanumeric)
                    .take(65_536)
                    .map(char::from)
                    .collect::<String>();
                self.0.fetch_add(data.len(), Ordering::Relaxed);

                Poll::Ready(Some(Ok(Bytes::from(data))))
            }
        }

        let (client, server) = Io::create();
        let mut h1 = h1(server, move |_| {
            let n = num2.clone();
            Box::pin(async move {
                Ok::<_, io::Error>(Response::Ok().message_body(Stream(n.clone())))
            })
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
        assert_eq!(state.with_write_buf(|buf| buf.len()).unwrap(), 65629);

        client.remote_buffer_cap(65536);
        sleep(Millis(50)).await;
        assert_eq!(state.with_write_buf(|buf| buf.len()).unwrap(), 93);

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
            ) -> Poll<Option<Result<Bytes, Box<dyn error::Error>>>> {
                if self.0 {
                    Poll::Pending
                } else {
                    self.0 = true;
                    let data = rand::thread_rng()
                        .sample_iter(&rand::distributions::Alphanumeric)
                        .take(1024)
                        .map(char::from)
                        .collect::<String>();
                    Poll::Ready(Some(Ok(Bytes::from(data))))
                }
            }
        }

        let (client, server) = Io::create();
        client.remote_buffer_cap(4096);
        let mut h1 = h1(server, |_| {
            Box::pin(async {
                Ok::<_, io::Error>(Response::Ok().message_body(Stream(false)))
            })
        });

        client.write("GET /test HTTP/1.1\r\n\r\n");
        sleep(Millis(50)).await;
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());

        // http message must be consumed
        assert_eq!(client.remote_buffer(|buf| buf.len()), 0);

        let mut decoder = ClientCodec::default();
        let mut buf = BytesMut::from(&client.read().await.unwrap()[..]);
        assert!(load(&mut decoder, &mut buf).status.is_success());
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());

        client.close().await;
        sleep(Millis(50)).await;
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_ready());
    }

    #[crate::rt_test]
    async fn test_service_error() {
        let (client, server) = Io::create();
        client.remote_buffer_cap(4096);
        client.write("GET /test HTTP/1.1\r\ncontent-length:512\r\n\r\n");

        let mut h1 = h1(server, |_| {
            Box::pin(async {
                Err::<Response<()>, _>(io::Error::new(io::ErrorKind::Other, "error"))
            })
        });
        // required because io shutdown is async oper
        assert!(poll_fn(|cx| Pin::new(&mut h1).poll(cx)).await.is_ok());

        assert!(h1.inner.io.is_closed());
        let buf = client.local_buffer(|buf| buf.split());
        assert_eq!(&buf[..28], b"HTTP/1.1 500 Internal Server");
        assert_eq!(&buf[buf.len() - 5..], b"error");
    }

    #[crate::rt_test]
    async fn test_payload_timeout() {
        let mark = Arc::new(AtomicUsize::new(0));
        let mark2 = mark.clone();
        let err_mark = Arc::new(AtomicUsize::new(0));
        let err_mark2 = err_mark.clone();

        let (client, server) = Io::create();
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
                        return Ok::<_, io::Error>(Response::Ok().finish());
                    }
                }
                Ok::<_, io::Error>(Response::Ok().finish())
            }
        };

        let mut config = ServiceConfig::new(
            Seconds(5).into(),
            Seconds(1),
            Seconds::ZERO,
            Millis(5_000),
            Config::server(),
        );
        config.payload_read_rate(Seconds(1), Seconds(2), 512);
        let disp: Dispatcher<Base, _, _, _> = Dispatcher::new(
            nio::Io::new(server),
            Rc::new(DispatcherConfig::new(
                config,
                svc.into_service(),
                fn_service(move |msg: Control<_, _>| {
                    if let Control::ProtocolError(ref err) = msg {
                        if matches!(err.err(), ProtocolError::SlowPayloadTimeout) {
                            err_mark2.store(
                                err_mark2.load(Ordering::Relaxed) + 1,
                                Ordering::Relaxed,
                            );
                        }
                    }
                    async move { Ok::<_, io::Error>(msg.ack()) }
                }),
            )),
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
        assert!(mark.load(Ordering::Relaxed) == 1536);
        assert!(err_mark.load(Ordering::Relaxed) == 1);
    }

    #[crate::rt_test]
    async fn test_unconsumed_payload() {
        let (client, server) = Io::create();
        client.remote_buffer_cap(4096);
        client.write("GET /test HTTP/1.1\r\ncontent-length:512\r\n\r\n");

        let mut h1 = h1(server, |_| {
            Box::pin(async { Ok::<_, io::Error>(Response::Ok().body("TEST")) })
        });
        // required because io shutdown is async oper
        assert!(poll_fn(|cx| Pin::new(&mut h1).poll(cx)).await.is_ok());

        assert!(h1.inner.io.is_closed());
        let buf = client.local_buffer(|buf| buf.split());
        assert_eq!(&buf[..15], b"HTTP/1.1 200 OK");
    }
}
