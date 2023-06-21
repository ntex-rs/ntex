//! Framed transport dispatcher
use std::task::{Context, Poll};
use std::{cell::RefCell, error::Error, future::Future, io, marker, pin::Pin, rc::Rc};

use crate::io::{Filter, Io, IoBoxed, IoRef, IoStatusUpdate, RecvError};
use crate::service::{Container, ContainerCall, Service};
use crate::util::{ready, Bytes};

use crate::http;
use crate::http::body::{BodySize, MessageBody, ResponseBody};
use crate::http::config::{DispatcherConfig, OnRequest};
use crate::http::error::{DispatchError, ParseError, PayloadError, ResponseError};
use crate::http::message::{ConnectionType, CurrentIo};
use crate::http::request::Request;
use crate::http::response::Response;

use super::decoder::{PayloadDecoder, PayloadItem, PayloadType};
use super::payload::{Payload, PayloadSender, PayloadStatus};
use super::{codec::Codec, Message};

bitflags::bitflags! {
    pub struct Flags: u16 {
        /// We parsed one complete request message
        const STARTED              = 0b0000_0001;
        /// Keep-alive is enabled on current connection
        const KEEPALIVE            = 0b0000_0010;
        /// Keep-alive is registered
        const KEEPALIVE_REG        = 0b0000_0100;
        /// Upgrade request
        const UPGRADE              = 0b0000_1000;
        /// Handling upgrade
        const UPGRADE_HND          = 0b0001_0000;
        /// Stop after sending payload
        const SENDPAYLOAD_AND_STOP = 0b0010_0000;
    }
}

pin_project_lite::pin_project! {
    /// Dispatcher for HTTP/1.1 protocol
    pub struct Dispatcher<F, S: Service<Request>, B, X: Service<Request>, U: Service<(Request, Io<F>, Codec)>>
    where S: 'static, X: 'static, U: 'static
    {
        #[pin]
        call: CallState<S, X>,
        st: State<B>,
        inner: DispatcherInner<F, S, B, X, U>,
    }
}

#[derive(Debug, thiserror::Error)]
enum State<B> {
    #[error("State::Call")]
    Call,
    #[error("State::ReadRequest")]
    ReadRequest,
    #[error("State::ReadFirstRequest")]
    ReadFirstRequest,
    #[error("State::ReadPayload")]
    ReadPayload,
    #[error("State::SendPayload")]
    SendPayload { body: ResponseBody<B> },
    #[error("State::SendPayloadAndStop")]
    SendPayloadAndStop {
        body: ResponseBody<B>,
        boxed_io: Option<Box<(IoBoxed, Codec)>>,
    },
    #[error("State::Upgrade")]
    Upgrade(Option<Request>),
    #[error("State::StopIo")]
    StopIo(Box<(IoBoxed, Codec)>),
    #[error("State::Stop")]
    Stop,
}

pin_project_lite::pin_project! {
    #[project = CallStateProject]
    enum CallState<S: Service<Request>, X: Service<Request>>
    where S: 'static, X: 'static
    {
        None,
        Service { #[pin] fut: ContainerCall<'static, S, Request> },
        ServiceUpgrade { #[pin] fut: ContainerCall<'static, S, Request>  },
        Expect { #[pin] fut: ContainerCall<'static, X, Request> },
        Filter { fut: ContainerCall<'static, OnRequest, (Request, IoRef)> }
    }
}

struct DispatcherInner<F, S, B, X, U> {
    io: Io<F>,
    flags: Flags,
    codec: Codec,
    config: Rc<DispatcherConfig<S, X, U>>,
    error: Option<DispatchError>,
    payload: Option<(PayloadDecoder, PayloadSender)>,
    _t: marker::PhantomData<(S, B)>,
}

impl<F, S, B, X, U> Dispatcher<F, S, B, X, U>
where
    F: Filter,
    S: Service<Request>,
    S::Error: ResponseError,
    S::Response: Into<Response<B>>,
    B: MessageBody,
    X: Service<Request, Response = Request>,
    X::Error: ResponseError,
    U: Service<(Request, Io<F>, Codec), Response = ()>,
{
    /// Construct new `Dispatcher` instance with outgoing messages stream.
    pub(in crate::http) fn new(io: Io<F>, config: Rc<DispatcherConfig<S, X, U>>) -> Self {
        let codec = Codec::new(config.timer.clone(), config.keep_alive_enabled());
        io.set_disconnect_timeout(config.client_disconnect.into());

        // slow-request timer
        let flags = if config.client_timeout.is_zero() {
            Flags::empty()
        } else {
            io.start_keepalive_timer(config.client_timeout);
            Flags::KEEPALIVE_REG
        };

        Dispatcher {
            call: CallState::None,
            st: State::ReadFirstRequest,
            inner: DispatcherInner {
                io,
                flags,
                codec,
                config,
                error: None,
                payload: None,
                _t: marker::PhantomData,
            },
        }
    }
}

impl<F, S, B, X, U> Future for Dispatcher<F, S, B, X, U>
where
    F: Filter,
    S: Service<Request>,
    S::Error: ResponseError + 'static,
    S::Response: Into<Response<B>>,
    B: MessageBody,
    X: Service<Request, Response = Request>,
    X::Error: ResponseError + 'static,
    U: Service<(Request, Io<F>, Codec), Response = ()> + 'static,
{
    type Output = Result<(), DispatchError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.as_mut().project();

        loop {
            match this.st {
                State::Call => {
                    let next = match this.call.project() {
                        CallStateProject::Service { fut } => {
                            match fut.poll(cx) {
                                Poll::Ready(result) => match result {
                                    Ok(res) => {
                                        let (res, body) = res.into().into_parts();
                                        *this.st = this.inner.send_response(res, body);
                                    }
                                    Err(e) => *this.st = this.inner.handle_error(e, false),
                                },
                                Poll::Pending => {
                                    // we might need to read more data into a request payload
                                    // (ie service future can wait for payload data)
                                    if this.inner.payload.is_some() {
                                        if let Err(e) =
                                            ready!(this.inner.poll_request_payload(cx))
                                        {
                                            *this.st = State::Stop;
                                            this.inner.error = Some(e);
                                        }
                                    } else if this.inner.poll_io_closed(cx) {
                                        // check if io is closed
                                        *this.st = State::Stop;
                                    } else {
                                        return Poll::Pending;
                                    }
                                }
                            }
                            None
                        }
                        // special handling for upgrade requests.
                        // we cannot continue to handle requests, because Io<F> get
                        // converted to IoBoxed before we set it to request,
                        // so we have to send response and disconnect. request payload
                        // handling should be handled by service
                        CallStateProject::ServiceUpgrade { fut } => {
                            match ready!(fut.poll(cx)) {
                                Ok(res) => {
                                    let (msg, body) = res.into().into_parts();
                                    let io = if let Some(item) = msg.head().take_io() {
                                        item
                                    } else {
                                        log::trace!("Handler service consumed io, stop");
                                        return Poll::Ready(Ok(()));
                                    };

                                    io.1.set_ctype(ConnectionType::Close);
                                    io.1.unset_streaming();
                                    let result = io
                                        .0
                                        .encode(Message::Item((msg, body.size())), &io.1);
                                    if result.is_ok() {
                                        match body.size() {
                                            BodySize::None | BodySize::Empty => {
                                                *this.st = State::StopIo(io)
                                            }
                                            _ => {
                                                *this.st = State::SendPayloadAndStop {
                                                    body,
                                                    boxed_io: Some(io),
                                                }
                                            }
                                        }
                                    } else {
                                        *this.st = State::StopIo(io);
                                    }
                                }
                                Err(e) => {
                                    log::error!(
                                        "Cannot handle error for upgrade handler: {:?}",
                                        e
                                    );
                                    return Poll::Ready(Ok(()));
                                }
                            }
                            None
                        }
                        // handle EXPECT call
                        // expect service call must resolve before
                        // we can do any more io processing.
                        //
                        // TODO: check keep-alive timer interaction
                        CallStateProject::Expect { fut } => match ready!(fut.poll(cx)) {
                            Ok(req) => {
                                let result = this.inner.io.with_write_buf(|buf| {
                                    buf.extend_from_slice(b"HTTP/1.1 100 Continue\r\n\r\n")
                                });
                                if result.is_err() {
                                    log::error!(
                                        "Expect handler returned error: {:?}",
                                        result.err().unwrap()
                                    );
                                    *this.st = State::Stop;
                                    this = self.as_mut().project();
                                    continue;
                                } else if this.inner.flags.contains(Flags::UPGRADE) {
                                    *this.st = State::Upgrade(Some(req));
                                    this = self.as_mut().project();
                                    continue;
                                } else if this.inner.flags.contains(Flags::UPGRADE_HND) {
                                    Some(this.inner.service_upgrade(req))
                                } else {
                                    Some(this.inner.service_call(req))
                                }
                            }
                            Err(e) => {
                                *this.st = this.inner.handle_error(e, true);
                                None
                            }
                        },
                        // handle FILTER call
                        CallStateProject::Filter { fut } => {
                            match ready!(Pin::new(fut).poll(cx)) {
                                Ok(req) => {
                                    this.inner
                                        .codec
                                        .set_ctype(req.head().connection_type());
                                    if req.head().expect() {
                                        Some(this.inner.service_expect(req))
                                    } else if this.inner.flags.contains(Flags::UPGRADE_HND)
                                    {
                                        Some(this.inner.service_upgrade(req))
                                    } else {
                                        Some(this.inner.service_call(req))
                                    }
                                }
                                Err(res) => {
                                    let (res, body) = res.into_parts();
                                    *this.st =
                                        this.inner.send_response(res, body.into_body());
                                    None
                                }
                            }
                        }
                        CallStateProject::None => unreachable!(),
                    };

                    this = self.as_mut().project();
                    if let Some(next) = next {
                        this.call.set(next);
                    }
                }
                // read request and call service
                State::ReadRequest => {
                    *this.st = ready!(this.inner.read_request(cx, &mut this.call));
                }
                // consume request's payload
                State::ReadPayload => {
                    if let Err(e) = ready!(this.inner.poll_request_payload(cx)) {
                        *this.st = State::Stop;
                        this.inner.error = Some(e);
                    } else {
                        *this.st = this.inner.switch_to_read_request();
                    }
                }
                // send response body
                State::SendPayload { ref mut body } => {
                    if this.inner.io.is_closed() {
                        *this.st = State::Stop;
                    } else {
                        if let Poll::Ready(Err(err)) = this.inner.poll_request_payload(cx) {
                            this.inner.error = Some(err);
                            this.inner.flags.insert(Flags::SENDPAYLOAD_AND_STOP);
                        }
                        loop {
                            let _ = ready!(this.inner.io.poll_flush(cx, false));
                            let item = ready!(body.poll_next_chunk(cx));
                            if let Some(st) = this.inner.send_payload(item) {
                                *this.st = st;
                                break;
                            }
                        }
                    }
                }
                // send response body
                State::SendPayloadAndStop {
                    ref mut body,
                    ref mut boxed_io,
                } => {
                    let io = boxed_io.as_ref().unwrap();

                    if io.0.is_closed() {
                        *this.st = State::Stop;
                    } else {
                        if let Poll::Ready(Err(err)) =
                            _poll_request_payload(&io.0, &mut this.inner.payload, cx)
                        {
                            this.inner.error = Some(err);
                        }
                        loop {
                            let _ = ready!(io.0.poll_flush(cx, false));
                            let item = ready!(body.poll_next_chunk(cx));
                            match item {
                                Some(Ok(item)) => {
                                    trace!("got response chunk: {:?}", item.len());
                                    if let Err(e) =
                                        io.0.encode(Message::Chunk(Some(item)), &io.1)
                                    {
                                        trace!("Cannot encode chunk: {:?}", e);
                                    } else {
                                        continue;
                                    }
                                }
                                None => {
                                    trace!("response payload eof {:?}", this.inner.flags);
                                    if let Err(e) = io.0.encode(Message::Chunk(None), &io.1)
                                    {
                                        trace!("Cannot encode payload eof: {:?}", e);
                                    }
                                }
                                Some(Err(e)) => {
                                    trace!("error during response body poll: {:?}", e);
                                }
                            }
                            *this.st = State::StopIo(boxed_io.take().unwrap());
                            break;
                        }
                    }
                }
                // read first request and call service
                State::ReadFirstRequest => {
                    *this.st = ready!(this.inner.read_request(cx, &mut this.call));
                    this.inner.flags.insert(Flags::STARTED);
                }
                // stop io tasks and call upgrade service
                State::Upgrade(ref mut req) => {
                    let io = this.inner.io.take();
                    let req = req.take().unwrap();

                    log::trace!("switching to upgrade service for {:?}", req);

                    // Handle UPGRADE request
                    let config = this.inner.config.clone();
                    let codec = this.inner.codec.clone();
                    crate::rt::spawn(async move {
                        let _ = config
                            .upgrade
                            .as_ref()
                            .unwrap()
                            .call((req, io, codec))
                            .await;
                    });
                    return Poll::Ready(Ok(()));
                }
                // prepare to shutdown
                State::Stop => {
                    this.inner.unregister_keepalive();

                    return if let Err(e) = ready!(this.inner.io.poll_shutdown(cx)) {
                        // get io error
                        if let Some(e) = this.inner.error.take() {
                            Poll::Ready(Err(e))
                        } else {
                            Poll::Ready(Err(DispatchError::PeerGone(Some(e))))
                        }
                    } else {
                        Poll::Ready(Ok(()))
                    };
                }
                // prepare to shutdown
                State::StopIo(ref item) => {
                    return item.0.poll_shutdown(cx).map_err(From::from)
                }
            }
        }
    }
}

impl<T, S, B, X, U> DispatcherInner<T, S, B, X, U>
where
    T: Filter,
    S: Service<Request>,
    S::Error: ResponseError + 'static,
    S::Response: Into<Response<B>>,
    B: MessageBody,
    X: Service<Request>,
{
    fn switch_to_read_request(&mut self) -> State<B> {
        // connection is not keep-alive, disconnect
        if !self.flags.contains(Flags::KEEPALIVE) || !self.codec.keepalive_enabled() {
            self.io.close();
            State::Stop
        } else {
            // register keep-alive timer
            if self.flags.contains(Flags::KEEPALIVE) {
                self.flags.remove(Flags::KEEPALIVE);
                self.flags.insert(Flags::KEEPALIVE_REG);
                self.io.start_keepalive_timer(self.config.keep_alive);
            }
            State::ReadRequest
        }
    }

    fn unregister_keepalive(&mut self) {
        if self.flags.contains(Flags::KEEPALIVE_REG) {
            self.io.stop_keepalive_timer();
            self.flags.remove(Flags::KEEPALIVE | Flags::KEEPALIVE_REG);
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
            if matches!(state, State::SendPayload { .. }) {
                self.flags.insert(Flags::SENDPAYLOAD_AND_STOP);
                state
            } else {
                State::Stop
            }
        } else {
            state
        }
    }

    fn service_call(&self, req: Request) -> CallState<S, X> {
        // Handle normal requests
        CallState::Service {
            fut: self.config.service.container_call(req).into_static(),
        }
    }

    fn service_filter(&self, req: Request, f: &Container<OnRequest>) -> CallState<S, X> {
        // Handle filter fut
        CallState::Filter {
            fut: f.container_call((req, self.io.get_ref())).into_static(),
        }
    }

    fn service_expect(&self, req: Request) -> CallState<S, X> {
        // Handle normal requests with EXPECT: 100-Continue` header
        CallState::Expect {
            fut: self.config.expect.container_call(req).into_static(),
        }
    }

    fn service_upgrade(&mut self, mut req: Request) -> CallState<S, X> {
        // Move io into request
        let io: IoBoxed = self.io.take().into();
        req.head_mut().io = CurrentIo::Io(Rc::new((
            io.get_ref(),
            RefCell::new(Some(Box::new((io, self.codec.clone())))),
        )));
        // Handle upgrade requests
        CallState::ServiceUpgrade {
            fut: self.config.service.container_call(req).into_static(),
        }
    }

    fn read_request(
        &mut self,
        cx: &mut Context<'_>,
        call_state: &mut std::pin::Pin<&mut CallState<S, X>>,
    ) -> Poll<State<B>> {
        log::trace!("trying to read http message");

        loop {
            let result = ready!(self.io.poll_recv(&self.codec, cx));

            // decode incoming bytes stream
            return match result {
                Ok((mut req, pl)) => {
                    log::trace!("http message is received: {:?} and payload {:?}", req, pl);

                    // keep-alive timer
                    if self.flags.contains(Flags::KEEPALIVE_REG) {
                        self.flags.remove(Flags::KEEPALIVE_REG);
                        self.io.stop_keepalive_timer();
                    }

                    // configure request payload
                    let upgrade = match pl {
                        PayloadType::None => false,
                        PayloadType::Payload(decoder) => {
                            let (ps, pl) = Payload::create(false);
                            req.replace_payload(http::Payload::H1(pl));
                            self.payload = Some((decoder, ps));
                            false
                        }
                        PayloadType::Stream(decoder) => {
                            if self.config.upgrade.is_none() {
                                let (ps, pl) = Payload::create(false);
                                req.replace_payload(http::Payload::H1(pl));
                                self.payload = Some((decoder, ps));
                                false
                            } else {
                                self.flags.insert(Flags::UPGRADE);
                                true
                            }
                        }
                    };

                    if upgrade {
                        // Handle UPGRADE request
                        log::trace!("prep io for upgrade handler");
                        Poll::Ready(State::Upgrade(Some(req)))
                    } else {
                        if req.upgrade() {
                            self.flags.insert(Flags::UPGRADE_HND);
                        } else {
                            req.head_mut().io = CurrentIo::Ref(self.io.get_ref());
                        }
                        call_state.set(if let Some(ref f) = self.config.on_request {
                            self.service_filter(req, f)
                        } else if req.head().expect() {
                            self.service_expect(req)
                        } else if self.flags.contains(Flags::UPGRADE_HND) {
                            self.service_upgrade(req)
                        } else {
                            self.service_call(req)
                        });
                        Poll::Ready(State::Call)
                    }
                }
                Err(RecvError::WriteBackpressure) => {
                    if let Err(err) = ready!(self.io.poll_flush(cx, false)) {
                        log::trace!("peer is gone with {:?}", err);
                        self.error = Some(DispatchError::PeerGone(Some(err)));
                        Poll::Ready(State::Stop)
                    } else {
                        continue;
                    }
                }
                Err(RecvError::Decoder(err)) => {
                    // Malformed requests, respond with 400
                    log::trace!("malformed request: {:?}", err);
                    let (res, body) = Response::BadRequest().finish().into_parts();
                    self.error = Some(DispatchError::Parse(err));
                    Poll::Ready(self.send_response(res, body.into_body()))
                }
                Err(RecvError::PeerGone(err)) => {
                    log::trace!("peer is gone with {:?}", err);
                    self.error = Some(DispatchError::PeerGone(err));
                    Poll::Ready(State::Stop)
                }
                Err(RecvError::Stop) => {
                    log::trace!("dispatcher is instructed to stop");
                    Poll::Ready(State::Stop)
                }
                Err(RecvError::KeepAlive) => {
                    // keep-alive timeout
                    if !self.flags.contains(Flags::STARTED) {
                        log::trace!("slow request timeout");
                        let (req, body) = Response::RequestTimeout().finish().into_parts();
                        let _ = self.send_response(req, body.into_body());
                        self.error = Some(DispatchError::SlowRequestTimeout);
                    } else {
                        log::trace!("keep-alive timeout, close connection");
                    }
                    Poll::Ready(State::Stop)
                }
            };
        }
    }

    fn send_response(&mut self, msg: Response<()>, body: ResponseBody<B>) -> State<B> {
        trace!("sending response: {:?} body: {:?}", msg, body.size());
        // we dont need to process responses if socket is disconnected
        // but we still want to handle requests with app service
        // so we skip response processing for droppped connection
        if self.io.is_closed() {
            State::Stop
        } else {
            let result = self
                .io
                .encode(Message::Item((msg, body.size())), &self.codec)
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
                            self.switch_to_read_request()
                        }
                    }
                    _ => State::SendPayload { body },
                }
            }
        }
    }

    fn send_payload(
        &mut self,
        item: Option<Result<Bytes, Box<dyn Error>>>,
    ) -> Option<State<B>> {
        match item {
            Some(Ok(item)) => {
                trace!("got response chunk: {:?}", item.len());
                match self.io.encode(Message::Chunk(Some(item)), &self.codec) {
                    Ok(_) => None,
                    Err(err) => {
                        self.error = Some(DispatchError::Encode(err));
                        Some(State::Stop)
                    }
                }
            }
            None => {
                trace!("response payload eof {:?}", self.flags);
                if let Err(err) = self.io.encode(Message::Chunk(None), &self.codec) {
                    self.error = Some(DispatchError::Encode(err));
                    Some(State::Stop)
                } else if self.flags.contains(Flags::SENDPAYLOAD_AND_STOP) {
                    Some(State::Stop)
                } else if self.payload.is_some() {
                    Some(State::ReadPayload)
                } else {
                    Some(self.switch_to_read_request())
                }
            }
            Some(Err(e)) => {
                trace!("error during response body poll: {:?}", e);
                self.error = Some(DispatchError::ResponsePayload(e));
                Some(State::Stop)
            }
        }
    }

    /// Process request's payload
    fn poll_request_payload(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), DispatchError>> {
        _poll_request_payload(&self.io, &mut self.payload, cx)
    }

    /// check for io changes, could close while waiting for service call
    fn poll_io_closed(&self, cx: &mut Context<'_>) -> bool {
        match self.io.poll_status_update(cx) {
            Poll::Pending => false,
            Poll::Ready(
                IoStatusUpdate::KeepAlive
                | IoStatusUpdate::Stop
                | IoStatusUpdate::PeerGone(_),
            ) => true,
            Poll::Ready(IoStatusUpdate::WriteBackpressure) => false,
        }
    }
}

/// Process request's payload
fn _poll_request_payload<F>(
    io: &Io<F>,
    slf_payload: &mut Option<(PayloadDecoder, PayloadSender)>,
    cx: &mut Context<'_>,
) -> Poll<Result<(), DispatchError>> {
    // check if payload data is required
    let payload = if let Some(ref mut payload) = slf_payload {
        payload
    } else {
        return Poll::Ready(Ok(()));
    };
    match payload.1.poll_data_required(cx) {
        PayloadStatus::Read => {
            // read request payload
            let mut updated = false;
            loop {
                match io.poll_recv(&payload.0, cx) {
                    Poll::Ready(Ok(PayloadItem::Chunk(chunk))) => {
                        updated = true;
                        payload.1.feed_data(chunk);
                    }
                    Poll::Ready(Ok(PayloadItem::Eof)) => {
                        updated = true;
                        payload.1.feed_eof();
                        *slf_payload = None;
                        break;
                    }
                    Poll::Ready(Err(err)) => {
                        let err = match err {
                            RecvError::WriteBackpressure => {
                                if io.poll_flush(cx, false)?.is_pending() {
                                    break;
                                } else {
                                    continue;
                                }
                            }
                            RecvError::KeepAlive => {
                                payload.1.set_error(PayloadError::EncodingCorrupted);
                                *slf_payload = None;
                                io::Error::new(io::ErrorKind::Other, "Keep-alive").into()
                            }
                            RecvError::Stop => {
                                payload.1.set_error(PayloadError::EncodingCorrupted);
                                *slf_payload = None;
                                io::Error::new(io::ErrorKind::Other, "Dispatcher stopped")
                                    .into()
                            }
                            RecvError::PeerGone(err) => {
                                payload.1.set_error(PayloadError::EncodingCorrupted);
                                *slf_payload = None;
                                if let Some(err) = err {
                                    DispatchError::PeerGone(Some(err))
                                } else {
                                    ParseError::Incomplete.into()
                                }
                            }
                            RecvError::Decoder(e) => {
                                payload.1.set_error(PayloadError::EncodingCorrupted);
                                *slf_payload = None;
                                DispatchError::Parse(e)
                            }
                        };
                        return Poll::Ready(Err(err));
                    }
                    Poll::Pending => break,
                }
            }
            if updated {
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            }
        }
        PayloadStatus::Pause => Poll::Pending,
        PayloadStatus::Dropped => {
            // service call is not interested in payload
            // wait until future completes and then close
            // connection
            *slf_payload = None;
            Poll::Ready(Err(DispatchError::PayloadIsNotConsumed))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::{cell::Cell, io, sync::Arc};

    use ntex_h2::Config;
    use rand::Rng;

    use super::*;
    use crate::http::config::{DispatcherConfig, ServiceConfig};
    use crate::http::h1::{ClientCodec, ExpectHandler, UpgradeHandler};
    use crate::http::{body, Request, ResponseHead, StatusCode};
    use crate::io::{self as nio, Base};
    use crate::service::{boxed, fn_service, IntoService};
    use crate::util::{lazy, poll_fn, stream_recv, Bytes, BytesMut};
    use crate::{codec::Decoder, testing::Io, time::sleep, time::Millis, time::Seconds};

    const BUFFER_SIZE: usize = 32_768;

    /// Create http/1 dispatcher.
    pub(crate) fn h1<F, S, B>(
        stream: Io,
        service: F,
    ) -> Dispatcher<Base, S, B, ExpectHandler, UpgradeHandler<Base>>
    where
        F: IntoService<S, Request>,
        S: Service<Request>,
        S::Error: ResponseError + 'static,
        S::Response: Into<Response<B>>,
        B: MessageBody,
    {
        let config = ServiceConfig::new(
            Seconds(5).into(),
            Millis(1_000),
            Seconds::ZERO,
            Millis(5_000),
            Config::server(),
        );
        Dispatcher::new(
            nio::Io::new(stream),
            Rc::new(DispatcherConfig::new(
                config,
                service.into_service(),
                ExpectHandler,
                None,
                None,
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
        crate::rt::spawn(
            Dispatcher::<Base, S, B, ExpectHandler, UpgradeHandler<Base>>::new(
                nio::Io::new(stream),
                Rc::new(DispatcherConfig::new(
                    ServiceConfig::default(),
                    service.into_service(),
                    ExpectHandler,
                    None,
                    None,
                )),
            ),
        );
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
            Millis(1_000),
            Seconds::ZERO,
            Millis(5_000),
            Config::server(),
        );
        let mut h1 = Dispatcher::<_, _, _, _, UpgradeHandler<Base>>::new(
            nio::Io::new(server),
            Rc::new(DispatcherConfig::new(
                config,
                fn_service(|_| {
                    Box::pin(async { Ok::<_, io::Error>(Response::Ok().finish()) })
                }),
                ExpectHandler,
                None,
                Some(boxed::service(crate::service::into_service(
                    move |(req, _)| {
                        data2.set(true);
                        Box::pin(async move { Ok(req) })
                    },
                ))),
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
        env_logger::init();
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
        crate::util::poll_fn(|cx| Pin::new(&mut h1).poll(cx))
            .await
            .unwrap();
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
            ) -> Poll<Option<Result<Bytes, Box<dyn std::error::Error>>>> {
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
            ) -> Poll<Option<Result<Bytes, Box<dyn std::error::Error>>>> {
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
}
