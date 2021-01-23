//! Framed transport dispatcher
use std::error::Error;
use std::task::{Context, Poll};
use std::{
    cell::RefCell, fmt, future::Future, marker::PhantomData, net, pin::Pin, rc::Rc,
    time::Duration, time::Instant,
};

use bytes::Bytes;

use crate::codec::{AsyncRead, AsyncWrite, Decoder};
use crate::framed::{ReadTask, State as IoState, WriteTask};
use crate::service::Service;

use crate::http;
use crate::http::body::{BodySize, MessageBody, ResponseBody};
use crate::http::config::DispatcherConfig;
use crate::http::error::{DispatchError, PayloadError, ResponseError};
use crate::http::helpers::DataFactory;
use crate::http::request::Request;
use crate::http::response::Response;

use super::decoder::{PayloadDecoder, PayloadItem, PayloadType};
use super::payload::{Payload, PayloadSender, PayloadStatus};
use super::{codec::Codec, Message};

bitflags::bitflags! {
    pub struct Flags: u16 {
        /// We parsed one complete request message
        const STARTED       = 0b0000_0001;
        /// Keep-alive is enabled on current connection
        const KEEPALIVE     = 0b0000_0010;
        /// Upgrade request
        const UPGRADE       = 0b0000_0100;
    }
}

pin_project_lite::pin_project! {
    /// Dispatcher for HTTP/1.1 protocol
    pub struct Dispatcher<S: Service, B, X: Service, U: Service> {
        #[pin]
        call: CallState<S, X, U>,
        st: State<B>,
        inner: DispatcherInner<S, B, X, U>,
    }
}

enum State<B> {
    Call,
    ReadRequest,
    ReadPayload,
    SendPayload { body: ResponseBody<B> },
    Stop,
}

pin_project_lite::pin_project! {
    #[project = CallStateProject]
    enum CallState<S: Service, X: Service, U: Service> {
        None,
        Service { #[pin] fut: S::Future },
        Expect { #[pin] fut: X::Future },
        Upgrade { #[pin] fut: U::Future },
    }
}

struct DispatcherInner<S, B, X, U> {
    flags: Flags,
    codec: Codec,
    config: Rc<DispatcherConfig<S, X, U>>,
    state: IoState,
    expire: Instant,
    error: Option<DispatchError>,
    payload: Option<(PayloadDecoder, PayloadSender)>,
    peer_addr: Option<net::SocketAddr>,
    on_connect_data: Option<Box<dyn DataFactory>>,
    _t: PhantomData<(S, B)>,
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum PollPayloadStatus {
    Done,
    Updated,
    Pending,
    Dropped,
}

impl<S, B, X, U> Dispatcher<S, B, X, U>
where
    S: Service<Request = Request>,
    S::Error: ResponseError + 'static,
    S::Response: Into<Response<B>>,
    B: MessageBody,
    X: Service<Request = Request, Response = Request>,
    X::Error: ResponseError,
    U: Service<Request = (Request, IoState, Codec), Response = ()>,
    U::Error: Error + fmt::Display,
{
    /// Construct new `Dispatcher` instance with outgoing messages stream.
    pub(in crate::http) fn new<T>(
        io: T,
        config: Rc<DispatcherConfig<S, X, U>>,
        peer_addr: Option<net::SocketAddr>,
        on_connect_data: Option<Box<dyn DataFactory>>,
    ) -> Self
    where
        T: AsyncRead + AsyncWrite + Unpin + 'static,
    {
        let codec = Codec::new(config.timer.clone(), config.keep_alive_enabled());

        let state = IoState::new();
        state.set_disconnect_timeout(config.client_disconnect as u16);

        let mut expire = config.timer_h1.now();
        let io = Rc::new(RefCell::new(io));

        // slow-request timer
        if config.client_timeout != 0 {
            expire += Duration::from_secs(config.client_timeout);
            config.timer_h1.register(expire, expire, &state);
        }

        // start support io tasks
        crate::rt::spawn(ReadTask::new(io.clone(), state.clone()));
        crate::rt::spawn(WriteTask::new(io, state.clone()));

        Dispatcher {
            call: CallState::None,
            st: State::ReadRequest,
            inner: DispatcherInner {
                flags: Flags::empty(),
                error: None,
                payload: None,
                codec,
                config,
                state,
                expire,
                peer_addr,
                on_connect_data,
                _t: PhantomData,
            },
        }
    }
}

impl<S, B, X, U> Future for Dispatcher<S, B, X, U>
where
    S: Service<Request = Request>,
    S::Error: ResponseError + 'static,
    S::Response: Into<Response<B>>,
    B: MessageBody,
    X: Service<Request = Request, Response = Request>,
    X::Error: ResponseError + 'static,
    U: Service<Request = (Request, IoState, Codec), Response = ()>,
    U::Error: Error + fmt::Display + 'static,
{
    type Output = Result<(), DispatchError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.as_mut().project();

        loop {
            match this.st {
                State::Call => {
                    let next = match this.call.project() {
                        // handle SERVICE call
                        CallStateProject::Service { fut } => {
                            // we have to loop because of read back-pressure,
                            // check Poll::Pending processing
                            match fut.poll(cx) {
                                Poll::Ready(result) => match result {
                                    Ok(res) => {
                                        let (res, body) = res.into().into_parts();
                                        *this.st = this.inner.send_response(res, body)
                                    }
                                    Err(e) => {
                                        *this.st = this.inner.handle_error(e, false)
                                    }
                                },
                                Poll::Pending => {
                                    // we might need to read more data into a request payload
                                    // (ie service future can wait for payload data)
                                    if this.inner.poll_read_payload(cx)
                                        != PollPayloadStatus::Updated
                                    {
                                        return Poll::Pending;
                                    }
                                }
                            }
                            None
                        }
                        // handle EXPECT call
                        CallStateProject::Expect { fut } => match fut.poll(cx) {
                            Poll::Ready(result) => match result {
                                Ok(req) => {
                                    this.inner.state.with_write_buf(|buf| {
                                        buf.extend_from_slice(
                                            b"HTTP/1.1 100 Continue\r\n\r\n",
                                        )
                                    });
                                    Some(if this.inner.flags.contains(Flags::UPGRADE) {
                                        // Handle UPGRADE request
                                        CallState::Upgrade {
                                            fut: this
                                                .inner
                                                .config
                                                .upgrade
                                                .as_ref()
                                                .unwrap()
                                                .call((
                                                    req,
                                                    this.inner.state.clone(),
                                                    this.inner.codec.clone(),
                                                )),
                                        }
                                    } else {
                                        CallState::Service {
                                            fut: this.inner.config.service.call(req),
                                        }
                                    })
                                }
                                Err(e) => {
                                    *this.st = this.inner.handle_error(e, true);
                                    None
                                }
                            },
                            Poll::Pending => {
                                // expect service call must resolve before
                                // we can do any more io processing.
                                //
                                // TODO: check keep-alive timer interaction
                                return Poll::Pending;
                            }
                        },
                        CallStateProject::Upgrade { fut } => {
                            return fut.poll(cx).map_err(|e| {
                                error!("Upgrade handler error: {}", e);
                                DispatchError::Upgrade(Box::new(e))
                            });
                        }
                        CallStateProject::None => unreachable!(),
                    };

                    this = self.as_mut().project();
                    if let Some(next) = next {
                        this.call.set(next);
                    }
                }
                State::ReadRequest => {
                    // stop dispatcher
                    if this.inner.state.is_dsp_stopped() {
                        log::trace!("dispatcher is instructed to stop");
                        *this.st = State::Stop;
                        continue;
                    }

                    // keep-alive timeout
                    if this.inner.state.is_keepalive() {
                        if !this.inner.flags.contains(Flags::STARTED) {
                            log::trace!("slow request timeout");
                            let (req, body) =
                                Response::RequestTimeout().finish().into_parts();
                            let _ = this.inner.send_response(req, body.into_body());
                            this.inner.error = Some(DispatchError::SlowRequestTimeout);
                        } else {
                            log::trace!("keep-alive timeout, close connection");
                        }
                        *this.st = State::Stop;

                        continue;
                    }

                    // decode incoming bytes stream
                    if this.inner.state.is_read_ready() {
                        match this.inner.state.decode_item(&this.inner.codec) {
                            Ok(Some((mut req, pl))) => {
                                log::trace!("http message is received: {:?}", req);
                                req.head_mut().peer_addr = this.inner.peer_addr;

                                // configure request payload
                                let upgrade = match pl {
                                    PayloadType::None => false,
                                    PayloadType::Payload(decoder) => {
                                        let (ps, pl) = Payload::create(false);
                                        req.replace_payload(http::Payload::H1(pl));
                                        this.inner.payload = Some((decoder, ps));
                                        false
                                    }
                                    PayloadType::Stream(decoder) => {
                                        if this.inner.config.upgrade.is_none() {
                                            let (ps, pl) = Payload::create(false);
                                            req.replace_payload(http::Payload::H1(pl));
                                            this.inner.payload = Some((decoder, ps));
                                            false
                                        } else {
                                            this.inner.flags.insert(Flags::UPGRADE);
                                            true
                                        }
                                    }
                                };

                                // unregister slow-request timer
                                if !this.inner.flags.contains(Flags::STARTED) {
                                    this.inner.flags.insert(Flags::STARTED);
                                    this.inner.config.timer_h1.unregister(
                                        this.inner.expire,
                                        &this.inner.state,
                                    );
                                }

                                // set on_connect data
                                if let Some(ref on_connect) = this.inner.on_connect_data
                                {
                                    on_connect.set(&mut req.extensions_mut());
                                }

                                // call service
                                *this.st = State::Call;
                                this.call.set(if req.head().expect() {
                                    // Handle `EXPECT: 100-Continue` header
                                    CallState::Expect {
                                        fut: this.inner.config.expect.call(req),
                                    }
                                } else if upgrade {
                                    log::trace!("initate upgrade handling");
                                    // Handle UPGRADE request
                                    CallState::Upgrade {
                                        fut: this
                                            .inner
                                            .config
                                            .upgrade
                                            .as_ref()
                                            .unwrap()
                                            .call((
                                                req,
                                                this.inner.state.clone(),
                                                this.inner.codec.clone(),
                                            )),
                                    }
                                } else {
                                    // Handle normal requests
                                    CallState::Service {
                                        fut: this.inner.config.service.call(req),
                                    }
                                });
                            }
                            Ok(None) => {
                                // if connection is not keep-alive then disconnect
                                if this.inner.flags.contains(Flags::STARTED)
                                    && !this.inner.flags.contains(Flags::KEEPALIVE)
                                {
                                    *this.st = State::Stop;
                                    continue;
                                }
                                this.inner.state.dsp_read_more_data(cx.waker());
                                return Poll::Pending;
                            }
                            Err(err) => {
                                // Malformed requests, respond with 400
                                let (res, body) =
                                    Response::BadRequest().finish().into_parts();
                                this.inner.error = Some(DispatchError::Parse(err));
                                *this.st =
                                    this.inner.send_response(res, body.into_body());
                                continue;
                            }
                        }
                    } else {
                        // if connection is not keep-alive then disconnect
                        if this.inner.flags.contains(Flags::STARTED)
                            && !this.inner.flags.contains(Flags::KEEPALIVE)
                        {
                            *this.st = State::Stop;
                            continue;
                        }
                        this.inner.state.dsp_register_task(cx.waker());
                        return Poll::Pending;
                    }
                }
                // consume request's payload
                State::ReadPayload => loop {
                    match this.inner.poll_read_payload(cx) {
                        PollPayloadStatus::Updated => continue,
                        PollPayloadStatus::Pending => return Poll::Pending,
                        PollPayloadStatus::Done => {
                            *this.st = {
                                this.inner.reset_keepalive();
                                State::ReadRequest
                            }
                        }
                        PollPayloadStatus::Dropped => *this.st = State::Stop,
                    }
                    break;
                },
                // send response body
                State::SendPayload { ref mut body } => {
                    this.inner.poll_read_payload(cx);

                    match body.poll_next_chunk(cx) {
                        Poll::Ready(item) => {
                            if let Some(st) = this.inner.send_payload(item) {
                                *this.st = st;
                            }
                        }
                        Poll::Pending => return Poll::Pending,
                    }
                }
                // prepare to shutdown
                State::Stop => {
                    this.inner.state.shutdown_io();
                    this.inner.unregister_keepalive();

                    // get io error
                    if this.inner.error.is_none() {
                        this.inner.error =
                            this.inner.state.take_io_error().map(DispatchError::Io);
                    }

                    return Poll::Ready(if let Some(err) = this.inner.error.take() {
                        Err(err)
                    } else {
                        Ok(())
                    });
                }
            }
        }
    }
}

impl<S, B, X, U> DispatcherInner<S, B, X, U>
where
    S: Service<Request = Request>,
    S::Error: ResponseError + 'static,
    S::Response: Into<Response<B>>,
    B: MessageBody,
{
    fn unregister_keepalive(&mut self) {
        if self.flags.contains(Flags::KEEPALIVE) {
            self.config.timer_h1.unregister(self.expire, &self.state);
        }
    }

    fn reset_keepalive(&mut self) {
        // re-register keep-alive
        if self.flags.contains(Flags::KEEPALIVE) {
            let expire =
                self.config.timer_h1.now() + Duration::from_secs(self.config.keep_alive);
            self.config
                .timer_h1
                .register(expire, self.expire, &self.state);
            self.expire = expire;
            self.state.reset_keepalive();
        }
    }

    fn handle_error<E>(&mut self, err: E, critical: bool) -> State<B>
    where
        E: ResponseError + 'static,
    {
        let res: Response = (&err).into();
        let (res, body) = res.into_parts();
        let state = self.send_response(res, body.into_body());

        // check if we can continue after error
        if critical || self.payload.take().is_some() {
            self.error = Some(DispatchError::Service(Box::new(err)));
            State::Stop
        } else {
            state
        }
    }

    fn send_response(&mut self, msg: Response<()>, body: ResponseBody<B>) -> State<B> {
        trace!("Sending response: {:?} body: {:?}", msg, body.size());
        // we dont need to process responses if socket is disconnected
        // but we still want to handle requests with app service
        // so we skip response processing for droppped connection
        if !self.state.is_io_err() {
            let result = self
                .state
                .write_item(Message::Item((msg, body.size())), &self.codec)
                .map_err(|err| {
                    if let Some(mut payload) = self.payload.take() {
                        payload.1.set_error(PayloadError::Incomplete(None));
                    }
                    err
                });

            if result.is_err() {
                State::Stop
            } else {
                self.flags.set(Flags::KEEPALIVE, self.codec.keepalive());

                match body.size() {
                    BodySize::None | BodySize::Empty => {
                        if self.error.is_some() {
                            State::Stop
                        } else if self.payload.is_some() {
                            State::ReadPayload
                        } else {
                            self.reset_keepalive();
                            State::ReadRequest
                        }
                    }
                    _ => State::SendPayload { body },
                }
            }
        } else {
            State::Stop
        }
    }

    fn send_payload(
        &mut self,
        item: Option<Result<Bytes, Box<dyn Error>>>,
    ) -> Option<State<B>> {
        match item {
            Some(Ok(item)) => {
                trace!("Got response chunk: {:?}", item.len());
                if let Err(err) = self
                    .state
                    .write_item(Message::Chunk(Some(item)), &self.codec)
                {
                    self.error = Some(DispatchError::Encode(err));
                    Some(State::Stop)
                } else {
                    None
                }
            }
            None => {
                trace!("Response payload eof");
                if let Err(err) =
                    self.state.write_item(Message::Chunk(None), &self.codec)
                {
                    self.error = Some(DispatchError::Encode(err));
                    Some(State::Stop)
                } else if self.payload.is_some() {
                    Some(State::ReadPayload)
                } else {
                    self.reset_keepalive();
                    Some(State::ReadRequest)
                }
            }
            Some(Err(e)) => {
                trace!("Error during response body poll: {:?}", e);
                self.error = Some(DispatchError::ResponsePayload(e));
                Some(State::Stop)
            }
        }
    }

    /// Process request's payload
    fn poll_read_payload(&mut self, cx: &mut Context<'_>) -> PollPayloadStatus {
        // check if payload data is required
        if let Some(ref mut payload) = self.payload {
            match payload.1.poll_data_required(cx) {
                PayloadStatus::Read => {
                    // read request payload
                    let mut updated = false;
                    loop {
                        let item = self.state.with_read_buf(|buf| payload.0.decode(buf));
                        match item {
                            Ok(Some(PayloadItem::Chunk(chunk))) => {
                                updated = true;
                                payload.1.feed_data(chunk);
                            }
                            Ok(Some(PayloadItem::Eof)) => {
                                payload.1.feed_eof();
                                self.payload = None;
                                if !updated {
                                    return PollPayloadStatus::Done;
                                }
                                break;
                            }
                            Ok(None) => {
                                self.state.dsp_read_more_data(cx.waker());
                                break;
                            }
                            Err(e) => {
                                payload.1.set_error(PayloadError::EncodingCorrupted);
                                self.payload = None;
                                self.error = Some(DispatchError::Parse(e));
                                return PollPayloadStatus::Dropped;
                            }
                        }
                    }
                    if updated {
                        PollPayloadStatus::Updated
                    } else {
                        PollPayloadStatus::Pending
                    }
                }
                PayloadStatus::Pause => PollPayloadStatus::Pending,
                PayloadStatus::Dropped => {
                    // service call is not interested in payload
                    // wait until future completes and then close
                    // connection
                    self.payload = None;
                    self.error = Some(DispatchError::PayloadIsNotConsumed);
                    PollPayloadStatus::Dropped
                }
            }
        } else {
            PollPayloadStatus::Done
        }
    }
}
