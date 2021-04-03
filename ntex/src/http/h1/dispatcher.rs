//! Framed transport dispatcher
use std::task::{Context, Poll};
use std::{
    cell::RefCell, error::Error, fmt, future::Future, marker, net, pin::Pin, rc::Rc,
    time,
};

use bytes::Bytes;

use crate::codec::{AsyncRead, AsyncWrite};
use crate::framed::{ReadTask, State as IoState, WriteTask};
use crate::service::Service;

use crate::http;
use crate::http::body::{BodySize, MessageBody, ResponseBody};
use crate::http::config::DispatcherConfig;
use crate::http::error::{DispatchError, ParseError, PayloadError, ResponseError};
use crate::http::helpers::DataFactory;
use crate::http::request::Request;
use crate::http::response::Response;

use super::decoder::{PayloadDecoder, PayloadItem, PayloadType};
use super::payload::{Payload, PayloadSender, PayloadStatus};
use super::{codec::Codec, Message};

bitflags::bitflags! {
    pub struct Flags: u16 {
        /// We parsed one complete request message
        const STARTED         = 0b0000_0001;
        /// Keep-alive is enabled on current connection
        const KEEPALIVE       = 0b0000_0010;
        /// Upgrade request
        const UPGRADE         = 0b0000_0100;
        /// Stop after sending payload
        const SENDPAYLOAD_AND_STOP = 0b0000_0100;
    }
}

pin_project_lite::pin_project! {
    /// Dispatcher for HTTP/1.1 protocol
    pub struct Dispatcher<T, S: Service, B, X: Service, U: Service> {
        #[pin]
        call: CallState<S, X, U>,
        st: State<B>,
        inner: DispatcherInner<T, S, B, X, U>,
    }
}

enum State<B> {
    Call,
    ReadRequest,
    ReadPayload,
    SendPayload { body: ResponseBody<B> },
    Upgrade(Option<Request>),
    Stop,
}

pin_project_lite::pin_project! {
    #[project = CallStateProject]
    enum CallState<S: Service, X: Service, U: Service> {
        None,
        Service { #[pin] fut: S::Future },
        Expect { #[pin] fut: X::Future },
        Upgrade { #[pin] fut: U::Future },
        Filter { fut: Pin<Box<dyn Future<Output = Result<Request, Response>>>> }
    }
}

struct DispatcherInner<T, S, B, X, U> {
    io: Option<Rc<RefCell<T>>>,
    flags: Flags,
    codec: Codec,
    config: Rc<DispatcherConfig<T, S, X, U>>,
    state: IoState,
    expire: time::Instant,
    error: Option<DispatchError>,
    payload: Option<(PayloadDecoder, PayloadSender)>,
    peer_addr: Option<net::SocketAddr>,
    on_connect_data: Option<Box<dyn DataFactory>>,
    _t: marker::PhantomData<(S, B)>,
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum ReadPayloadStatus {
    Done,
    Updated,
    Pending,
    Dropped,
}

enum WritePayloadStatus<B> {
    Next(State<B>),
    Pause,
    Continue,
}

impl<T, S, B, X, U> Dispatcher<T, S, B, X, U>
where
    T: AsyncRead + AsyncWrite + Unpin + 'static,
    S: Service<Request = Request>,
    S::Error: ResponseError + 'static,
    S::Response: Into<Response<B>>,
    B: MessageBody,
    X: Service<Request = Request, Response = Request>,
    X::Error: ResponseError,
    U: Service<Request = (Request, T, IoState, Codec), Response = ()>,
    U::Error: Error + fmt::Display,
{
    /// Construct new `Dispatcher` instance with outgoing messages stream.
    pub(in crate::http) fn new(
        io: T,
        config: Rc<DispatcherConfig<T, S, X, U>>,
        peer_addr: Option<net::SocketAddr>,
        on_connect_data: Option<Box<dyn DataFactory>>,
    ) -> Self {
        let codec = Codec::new(config.timer.clone(), config.keep_alive_enabled());
        let state = IoState::with_params(
            config.read_hw,
            config.write_hw,
            config.lw,
            config.client_disconnect as u16,
        );

        let mut expire = config.timer_h1.now();
        let io = Rc::new(RefCell::new(io));

        // slow-request timer
        if config.client_timeout != 0 {
            expire += time::Duration::from_secs(config.client_timeout);
            config.timer_h1.register(expire, expire, &state);
        }

        // start support io tasks
        crate::rt::spawn(ReadTask::new(io.clone(), state.clone()));
        crate::rt::spawn(WriteTask::new(io.clone(), state.clone()));

        Dispatcher {
            call: CallState::None,
            st: State::ReadRequest,
            inner: DispatcherInner {
                io: Some(io),
                flags: Flags::empty(),
                error: None,
                payload: None,
                codec,
                config,
                state,
                expire,
                peer_addr,
                on_connect_data,
                _t: marker::PhantomData,
            },
        }
    }
}

impl<T, S, B, X, U> Future for Dispatcher<T, S, B, X, U>
where
    T: AsyncRead + AsyncWrite + Unpin + 'static,
    S: Service<Request = Request>,
    S::Error: ResponseError + 'static,
    S::Response: Into<Response<B>>,
    B: MessageBody,
    X: Service<Request = Request, Response = Request>,
    X::Error: ResponseError + 'static,
    U: Service<Request = (Request, T, IoState, Codec), Response = ()>,
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
                                        != ReadPayloadStatus::Updated
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
                                    this.inner.state.write().with_buf(|buf| {
                                        buf.extend_from_slice(
                                            b"HTTP/1.1 100 Continue\r\n\r\n",
                                        )
                                    });
                                    if this.inner.flags.contains(Flags::UPGRADE) {
                                        this.inner.state.stop_io(cx.waker());
                                        *this.st = State::Upgrade(Some(req));
                                        return Poll::Pending;
                                    } else {
                                        Some(CallState::Service {
                                            fut: this.inner.config.service.call(req),
                                        })
                                    }
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
                        // handle FILTER call
                        CallStateProject::Filter { fut } => {
                            if let Poll::Ready(result) = Pin::new(fut).poll(cx) {
                                match result {
                                    Ok(req) => {
                                        if req.head().expect() {
                                            // Handle normal requests with EXPECT: 100-Continue` header
                                            Some(CallState::Expect {
                                                fut: this.inner.config.expect.call(req),
                                            })
                                        } else {
                                            // Handle normal requests
                                            Some(CallState::Service {
                                                fut: this.inner.config.service.call(req),
                                            })
                                        }
                                    }
                                    Err(res) => {
                                        let (res, body) = res.into_parts();
                                        *this.st = this
                                            .inner
                                            .send_response(res, body.into_body());
                                        None
                                    }
                                }
                            } else {
                                return Poll::Pending;
                            }
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
                    if this.inner.state.is_dispatcher_stopped() {
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

                    let read = this.inner.state.read();

                    // decode incoming bytes stream
                    if read.is_ready() {
                        match read.decode(&this.inner.codec) {
                            Ok(Some((mut req, pl))) => {
                                log::trace!(
                                    "http message is received: {:?} and payload {:?}",
                                    req,
                                    pl
                                );
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

                                if upgrade {
                                    // Handle UPGRADE request
                                    log::trace!("prep io for upgrade handler");
                                    this.inner.state.stop_io(cx.waker());
                                    *this.st = State::Upgrade(Some(req));
                                    return Poll::Pending;
                                } else {
                                    *this.st = State::Call;
                                    this.call.set(
                                        if let Some(ref f) = this.inner.config.on_request
                                        {
                                            // Handle filter fut
                                            CallState::Filter {
                                                fut: f.call((
                                                    req,
                                                    this.inner
                                                        .io
                                                        .as_ref()
                                                        .unwrap()
                                                        .clone(),
                                                )),
                                            }
                                        } else if req.head().expect() {
                                            // Handle normal requests with EXPECT: 100-Continue` header
                                            CallState::Expect {
                                                fut: this.inner.config.expect.call(req),
                                            }
                                        } else {
                                            // Handle normal requests
                                            CallState::Service {
                                                fut: this.inner.config.service.call(req),
                                            }
                                        },
                                    );
                                }
                            }
                            Ok(None) => {
                                log::trace!("not enough data to decode next frame, register dispatch task");

                                // if io error occured or connection is not keep-alive
                                // then disconnect
                                if this.inner.flags.contains(Flags::STARTED)
                                    && (!this.inner.flags.contains(Flags::KEEPALIVE)
                                        || !this.inner.codec.keepalive_enabled()
                                        || this.inner.state.is_io_err())
                                {
                                    *this.st = State::Stop;
                                    this.inner.state.dispatcher_stopped();
                                    continue;
                                }
                                this.inner.state.read().wake(cx.waker());
                                return Poll::Pending;
                            }
                            Err(err) => {
                                // Malformed requests, respond with 400
                                log::trace!("malformed request: {:?}", err);
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
                        this.inner.state.register_dispatcher(cx.waker());
                        return Poll::Pending;
                    }
                }
                // consume request's payload
                State::ReadPayload => {
                    if this.inner.state.is_io_err() {
                        *this.st = State::Stop;
                    } else {
                        loop {
                            match this.inner.poll_read_payload(cx) {
                                ReadPayloadStatus::Updated => continue,
                                ReadPayloadStatus::Pending => return Poll::Pending,
                                ReadPayloadStatus::Done => {
                                    *this.st = {
                                        this.inner.reset_keepalive();
                                        State::ReadRequest
                                    }
                                }
                                ReadPayloadStatus::Dropped => *this.st = State::Stop,
                            }
                            break;
                        }
                    }
                }
                // send response body
                State::SendPayload { ref mut body } => {
                    if this.inner.state.is_io_err() {
                        *this.st = State::Stop;
                    } else {
                        this.inner.poll_read_payload(cx);

                        match body.poll_next_chunk(cx) {
                            Poll::Ready(item) => match this.inner.send_payload(item) {
                                WritePayloadStatus::Next(st) => {
                                    *this.st = st;
                                }
                                WritePayloadStatus::Pause => {
                                    this.inner
                                        .state
                                        .write()
                                        .enable_backpressure(Some(cx.waker()));
                                    return Poll::Pending;
                                }
                                WritePayloadStatus::Continue => (),
                            },
                            Poll::Pending => return Poll::Pending,
                        }
                    }
                }
                // stop io tasks and call upgrade service
                State::Upgrade(ref mut req) => {
                    // check if all io tasks have been stopped
                    let io = if Rc::strong_count(this.inner.io.as_ref().unwrap()) == 1 {
                        if let Ok(io) = Rc::try_unwrap(this.inner.io.take().unwrap()) {
                            io.into_inner()
                        } else {
                            return Poll::Ready(Err(DispatchError::InternalError));
                        }
                    } else {
                        // wait next task stop
                        this.inner.state.register_dispatcher(cx.waker());
                        return Poll::Pending;
                    };
                    log::trace!("initate upgrade handling");

                    let req = req.take().unwrap();
                    *this.st = State::Call;
                    this.inner.state.reset_io_stop();

                    // Handle UPGRADE request
                    this.call.set(CallState::Upgrade {
                        fut: this.inner.config.upgrade.as_ref().unwrap().call((
                            req,
                            io,
                            this.inner.state.clone(),
                            this.inner.codec.clone(),
                        )),
                    });
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

impl<T, S, B, X, U> DispatcherInner<T, S, B, X, U>
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
        if self.flags.contains(Flags::KEEPALIVE) && self.config.keep_alive.as_secs() != 0
        {
            let expire = self.config.timer_h1.now() + self.config.keep_alive;
            if expire != self.expire {
                self.config
                    .timer_h1
                    .register(expire, self.expire, &self.state);
                self.expire = expire;
                self.state.reset_keepalive();
            }
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

    fn send_response(&mut self, msg: Response<()>, body: ResponseBody<B>) -> State<B> {
        trace!("Sending response: {:?} body: {:?}", msg, body.size());
        // we dont need to process responses if socket is disconnected
        // but we still want to handle requests with app service
        // so we skip response processing for droppped connection
        if !self.state.is_io_err() {
            let result = self
                .state
                .write()
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
    ) -> WritePayloadStatus<B> {
        match item {
            Some(Ok(item)) => {
                trace!("Got response chunk: {:?}", item.len());
                match self
                    .state
                    .write()
                    .encode(Message::Chunk(Some(item)), &self.codec)
                {
                    Err(err) => {
                        self.error = Some(DispatchError::Encode(err));
                        WritePayloadStatus::Next(State::Stop)
                    }
                    Ok(has_space) => {
                        if has_space {
                            WritePayloadStatus::Continue
                        } else {
                            WritePayloadStatus::Pause
                        }
                    }
                }
            }
            None => {
                trace!("Response payload eof");
                if let Err(err) =
                    self.state.write().encode(Message::Chunk(None), &self.codec)
                {
                    self.error = Some(DispatchError::Encode(err));
                    WritePayloadStatus::Next(State::Stop)
                } else if self.flags.contains(Flags::SENDPAYLOAD_AND_STOP) {
                    WritePayloadStatus::Next(State::Stop)
                } else if self.payload.is_some() {
                    WritePayloadStatus::Next(State::ReadPayload)
                } else {
                    self.reset_keepalive();
                    WritePayloadStatus::Next(State::ReadRequest)
                }
            }
            Some(Err(e)) => {
                trace!("Error during response body poll: {:?}", e);
                self.error = Some(DispatchError::ResponsePayload(e));
                WritePayloadStatus::Next(State::Stop)
            }
        }
    }

    /// Process request's payload
    fn poll_read_payload(&mut self, cx: &mut Context<'_>) -> ReadPayloadStatus {
        // check if payload data is required
        if let Some(ref mut payload) = self.payload {
            match payload.1.poll_data_required(cx) {
                PayloadStatus::Read => {
                    let read = self.state.read();

                    // read request payload
                    let mut updated = false;
                    loop {
                        let item = read.decode(&payload.0);
                        match item {
                            Ok(Some(PayloadItem::Chunk(chunk))) => {
                                updated = true;
                                payload.1.feed_data(chunk);
                            }
                            Ok(Some(PayloadItem::Eof)) => {
                                payload.1.feed_eof();
                                self.payload = None;
                                if !updated {
                                    return ReadPayloadStatus::Done;
                                }
                                break;
                            }
                            Ok(None) => {
                                if self.state.is_io_err() {
                                    payload.1.set_error(PayloadError::EncodingCorrupted);
                                    self.payload = None;
                                    self.error = Some(ParseError::Incomplete.into());
                                    return ReadPayloadStatus::Dropped;
                                } else {
                                    read.wake(cx.waker());
                                    break;
                                }
                            }
                            Err(e) => {
                                payload.1.set_error(PayloadError::EncodingCorrupted);
                                self.payload = None;
                                self.error = Some(DispatchError::Parse(e));
                                return ReadPayloadStatus::Dropped;
                            }
                        }
                    }
                    if updated {
                        ReadPayloadStatus::Updated
                    } else {
                        ReadPayloadStatus::Pending
                    }
                }
                PayloadStatus::Pause => ReadPayloadStatus::Pending,
                PayloadStatus::Dropped => {
                    // service call is not interested in payload
                    // wait until future completes and then close
                    // connection
                    self.payload = None;
                    self.error = Some(DispatchError::PayloadIsNotConsumed);
                    ReadPayloadStatus::Dropped
                }
            }
        } else {
            ReadPayloadStatus::Done
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::{cell::Cell, io, sync::Arc};

    use bytes::{Bytes, BytesMut};
    use rand::Rng;

    use super::*;
    use crate::http::config::{DispatcherConfig, ServiceConfig};
    use crate::http::h1::{ClientCodec, ExpectHandler, UpgradeHandler};
    use crate::http::{body, Request, ResponseHead, StatusCode};
    use crate::service::{boxed, fn_service, IntoService};
    use crate::{codec::Decoder, rt::time::sleep, testing::Io, util::lazy, util::next};

    const BUFFER_SIZE: usize = 32_768;

    /// Create http/1 dispatcher.
    pub(crate) fn h1<F, S, B>(
        stream: Io,
        service: F,
    ) -> Dispatcher<Io, S, B, ExpectHandler, UpgradeHandler<Io>>
    where
        F: IntoService<S>,
        S: Service<Request = Request>,
        S::Error: ResponseError + 'static,
        S::Response: Into<Response<B>>,
        B: MessageBody,
    {
        Dispatcher::new(
            stream,
            Rc::new(DispatcherConfig::new(
                ServiceConfig::default(),
                service.into_service(),
                ExpectHandler,
                None,
                None,
            )),
            None,
            None,
        )
    }

    pub(crate) fn spawn_h1<F, S, B>(stream: Io, service: F)
    where
        F: IntoService<S>,
        S: Service<Request = Request> + 'static,
        S::Error: ResponseError,
        S::Response: Into<Response<B>>,
        B: MessageBody + 'static,
    {
        crate::rt::spawn(
            Dispatcher::<Io, S, B, ExpectHandler, UpgradeHandler<Io>>::new(
                stream,
                Rc::new(DispatcherConfig::new(
                    ServiceConfig::default(),
                    service.into_service(),
                    ExpectHandler,
                    None,
                    None,
                )),
                None,
                None,
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
        let mut h1 = Dispatcher::<_, _, _, _, UpgradeHandler<Io>>::new(
            server,
            Rc::new(DispatcherConfig::new(
                ServiceConfig::default(),
                fn_service(|_| {
                    Box::pin(async { Ok::<_, io::Error>(Response::Ok().finish()) })
                }),
                ExpectHandler,
                None,
                Some(boxed::service(crate::into_service(move |(req, _)| {
                    data2.set(true);
                    Box::pin(async move { Ok(req) })
                }))),
            )),
            None,
            None,
        );
        sleep(time::Duration::from_millis(50)).await;

        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_ready());
        sleep(time::Duration::from_millis(50)).await;

        client.local_buffer(|buf| assert_eq!(&buf[..15], b"HTTP/1.0 200 OK"));
        client.close().await;
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
        sleep(time::Duration::from_millis(50)).await;

        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_ready());
        assert!(!h1.inner.state.is_open());
        sleep(time::Duration::from_millis(50)).await;

        client
            .local_buffer(|buf| assert_eq!(&buf[..26], b"HTTP/1.1 400 Bad Request\r\n"));

        client.close().await;
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_ready());
        // assert!(h1.inner.flags.contains(Flags::SHUTDOWN_IO));
        assert!(h1.inner.state.is_io_err());
    }

    #[crate::rt_test]
    async fn test_pipeline() {
        let (client, server) = Io::create();
        client.remote_buffer_cap(4096);
        let mut decoder = ClientCodec::default();
        spawn_h1(server, |_| async {
            Ok::<_, io::Error>(Response::Ok().finish())
        });

        client.write("GET /test HTTP/1.1\r\n\r\n");

        let mut buf = client.read().await.unwrap();
        assert!(load(&mut decoder, &mut buf).status.is_success());
        assert!(!client.is_server_dropped());

        client.write("GET /test HTTP/1.1\r\n\r\n");
        client.write("GET /test HTTP/1.1\r\n\r\n");

        let mut buf = client.read().await.unwrap();
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
            while let Some(_) = next(&mut p).await {}
            Ok::<_, io::Error>(Response::Ok().finish())
        });

        client.write("GET /test HTTP/1.1\r\ncontent-length: 5\r\n\r\n");
        sleep(time::Duration::from_millis(50)).await;
        client.write("xxxxx");

        let mut buf = client.read().await.unwrap();
        assert!(load(&mut decoder, &mut buf).status.is_success());
        assert!(!client.is_server_dropped());

        client.write("GET /test HTTP/1.1\r\n\r\n");

        let mut buf = client.read().await.unwrap();
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
            sleep(time::Duration::from_millis(100)).await;
            Ok::<_, io::Error>(Response::Ok().finish())
        });

        client.write("GET /test HTTP/1.1\r\n\r\n");

        let mut buf = client.read().await.unwrap();
        assert!(load(&mut decoder, &mut buf).status.is_success());
        assert!(!client.is_server_dropped());

        client.write("GET /test HTTP/1.1\r\n\r\n");
        client.write("GET /test HTTP/1.1\r\n\r\n");
        sleep(time::Duration::from_millis(50)).await;
        client.write("GET /test HTTP/1.1\r\n\r\n");

        let mut buf = client.read().await.unwrap();
        assert!(load(&mut decoder, &mut buf).status.is_success());

        let mut buf = client.read().await.unwrap();
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
    /// if socket is disconnected, h1 dispatcher does not process any data
    // /// h1 dispatcher still processes all incoming requests
    // /// but it does not write any data to socket
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
        assert_eq!(num.load(Ordering::Relaxed), 0);
    }

    #[crate::rt_test]
    async fn test_read_large_message() {
        let (client, server) = Io::create();
        client.remote_buffer_cap(4096);

        let mut h1 = h1(server, |_| {
            Box::pin(async { Ok::<_, io::Error>(Response::Ok().finish()) })
        });
        h1.inner.state.set_buffer_params(16 * 1024, 16 * 1024, 1024);

        let mut decoder = ClientCodec::default();

        let data = rand::thread_rng()
            .sample_iter(&rand::distributions::Alphanumeric)
            .take(70_000)
            .map(char::from)
            .collect::<String>();
        client.write("GET /test HTTP/1.1\r\nContent-Length: ");
        client.write(data);
        sleep(time::Duration::from_millis(50)).await;

        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        sleep(time::Duration::from_millis(50)).await;
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_ready());
        assert!(!h1.inner.state.is_open());

        let mut buf = client.read().await.unwrap();
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
                let _ = next(&mut pl).await.unwrap().unwrap();
                m.store(true, Ordering::Relaxed);
                // sleep
                sleep(time::Duration::from_secs(999_999)).await;
                Ok::<_, io::Error>(Response::Ok().finish())
            }
        });

        client.write("GET /test HTTP/1.1\r\nContent-Length: 1048576\r\n\r\n");
        sleep(time::Duration::from_millis(50)).await;

        // buf must be consumed
        assert_eq!(client.remote_buffer(|buf| buf.len()), 0);

        // io should be drained only by no more than MAX_BUFFER_SIZE
        let random_bytes: Vec<u8> =
            (0..1_048_576).map(|_| rand::random::<u8>()).collect();
        client.write(random_bytes);

        sleep(time::Duration::from_millis(50)).await;
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
        let state = h1.inner.state.clone();

        // do not allow to write to socket
        client.remote_buffer_cap(0);
        client.write("GET /test HTTP/1.1\r\n\r\n");
        sleep(time::Duration::from_millis(50)).await;
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());

        // buf must be consumed
        assert_eq!(client.remote_buffer(|buf| buf.len()), 0);

        // amount of generated data
        assert_eq!(num.load(Ordering::Relaxed), 65_536);

        // response message + chunking encoding
        assert_eq!(state.write().with_buf(|buf| buf.len()), 65629);

        client.remote_buffer_cap(65536);
        sleep(time::Duration::from_millis(50)).await;
        assert_eq!(state.write().with_buf(|buf| buf.len()), 93);

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
        sleep(time::Duration::from_millis(50)).await;
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());

        // http message must be consumed
        assert_eq!(client.remote_buffer(|buf| buf.len()), 0);

        let mut decoder = ClientCodec::default();
        let mut buf = client.read().await.unwrap();
        assert!(load(&mut decoder, &mut buf).status.is_success());
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());

        client.close().await;
        sleep(time::Duration::from_millis(50)).await;
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
        sleep(time::Duration::from_millis(50)).await;

        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_ready());
        sleep(time::Duration::from_millis(50)).await;
        assert!(h1.inner.state.is_io_err());
        let buf = client.local_buffer(|buf| buf.split().freeze());
        assert_eq!(&buf[..28], b"HTTP/1.1 500 Internal Server");
        assert_eq!(&buf[buf.len() - 5..], b"error");
    }
}
