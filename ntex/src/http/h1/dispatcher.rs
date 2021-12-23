//! Framed transport dispatcher
use std::task::{Context, Poll};
use std::{error::Error, fmt, future::Future, marker, pin::Pin, rc::Rc, time};

use crate::io::{Filter, Io, IoRef};
use crate::service::Service;
use crate::{time::now, util::ready, util::Bytes, util::Either};

use crate::http;
use crate::http::body::{BodySize, MessageBody, ResponseBody};
use crate::http::config::DispatcherConfig;
use crate::http::error::{DispatchError, ParseError, PayloadError, ResponseError};
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
    pub struct Dispatcher<F, S: Service<Request>, B, X: Service<Request>, U: Service<(Request, Io<F>, Codec)>> {
        #[pin]
        call: CallState<S, X>,
        st: State<B>,
        inner: DispatcherInner<F, S, B, X, U>,
    }
}

#[derive(derive_more::Display)]
enum State<B> {
    Call,
    ReadRequest,
    ReadPayload,
    #[display(fmt = "State::SendPayload")]
    SendPayload {
        body: ResponseBody<B>,
    },
    #[display(fmt = "State::Upgrade")]
    Upgrade(Option<Request>),
    Stop,
}

pin_project_lite::pin_project! {
    #[project = CallStateProject]
    enum CallState<S: Service<Request>, X: Service<Request>> {
        None,
        Service { #[pin] fut: S::Future },
        Expect { #[pin] fut: X::Future },
        Filter { fut: Pin<Box<dyn Future<Output = Result<Request, Response>>>> }
    }
}

struct DispatcherInner<F, S, B, X, U> {
    io: Option<Io<F>>,
    flags: Flags,
    codec: Codec,
    state: IoRef,
    config: Rc<DispatcherConfig<S, X, U>>,
    expire: time::Instant,
    error: Option<DispatchError>,
    payload: Option<(PayloadDecoder, PayloadSender)>,
    _t: marker::PhantomData<(S, B)>,
}

impl<F, S, B, X, U> Dispatcher<F, S, B, X, U>
where
    F: Filter + 'static,
    S: Service<Request>,
    S::Error: ResponseError + 'static,
    S::Response: Into<Response<B>>,
    B: MessageBody,
    X: Service<Request, Response = Request>,
    X::Error: ResponseError,
    U: Service<(Request, Io<F>, Codec), Response = ()> + 'static,
    U::Error: Error + fmt::Display,
{
    /// Construct new `Dispatcher` instance with outgoing messages stream.
    pub(in crate::http) fn new(io: Io<F>, config: Rc<DispatcherConfig<S, X, U>>) -> Self {
        let mut expire = now();
        let state = io.get_ref();
        let codec = Codec::new(config.timer.clone(), config.keep_alive_enabled());
        io.set_disconnect_timeout(config.client_disconnect.into());

        // slow-request timer
        if config.client_timeout.non_zero() {
            expire += time::Duration::from(config.client_timeout);
            config.timer_h1.register(expire, expire, &state);
        }

        Dispatcher {
            call: CallState::None,
            st: State::ReadRequest,
            inner: DispatcherInner {
                io: Some(io),
                flags: Flags::empty(),
                error: None,
                payload: None,
                codec,
                state,
                config,
                expire,
                _t: marker::PhantomData,
            },
        }
    }
}

macro_rules! set_error ({ $slf:tt, $err:ident } => {
    *$slf.st = State::Stop;
    $slf.inner.error = Some($err);
    $slf.inner.unregister_keepalive();
});

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
    U::Error: Error + fmt::Display,
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
                                        *this.st = this.inner.send_response(res, body)
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
                                            set_error!(this, e);
                                        }
                                    } else {
                                        return Poll::Pending;
                                    }
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
                                let result = this.inner.state.with_write_buf(|buf| {
                                    buf.extend_from_slice(b"HTTP/1.1 100 Continue\r\n\r\n")
                                });
                                if result.is_err() {
                                    *this.st = State::Stop;
                                    this.inner.unregister_keepalive();
                                    this = self.as_mut().project();
                                    continue;
                                } else if this.inner.flags.contains(Flags::UPGRADE) {
                                    *this.st = State::Upgrade(Some(req));
                                    this = self.as_mut().project();
                                    continue;
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
                        // handle FILTER call
                        CallStateProject::Filter { fut } => {
                            match ready!(Pin::new(fut).poll(cx)) {
                                Ok(req) => {
                                    this.inner
                                        .codec
                                        .set_ctype(req.head().connection_type());
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
                State::ReadRequest => {
                    log::trace!("trying to read http message");

                    // stop dispatcher
                    if this.inner.io().is_dispatcher_stopped() {
                        log::trace!("dispatcher is instructed to stop");
                        *this.st = State::Stop;
                        this.inner.unregister_keepalive();
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
                        this.inner.unregister_keepalive();
                        continue;
                    }

                    let io = this.inner.io();

                    // decode incoming bytes stream
                    match ready!(io.poll_recv(&this.inner.codec, cx)) {
                        Ok(Some((mut req, pl))) => {
                            log::trace!(
                                "http message is received: {:?} and payload {:?}",
                                req,
                                pl
                            );
                            req.head_mut().io = Some(io.get_ref());

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
                                this.inner
                                    .config
                                    .timer_h1
                                    .unregister(this.inner.expire, &this.inner.state);
                            }

                            if upgrade {
                                // Handle UPGRADE request
                                log::trace!("prep io for upgrade handler");
                                *this.st = State::Upgrade(Some(req));
                            } else {
                                *this.st = State::Call;
                                this.call.set(
                                    if let Some(ref f) = this.inner.config.on_request {
                                        // Handle filter fut
                                        CallState::Filter {
                                            fut: f.call((req, this.inner.state.clone())),
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
                            // peer is gone
                            log::trace!("peer is gone");
                            let e = DispatchError::Disconnect(None);
                            set_error!(this, e);
                        }
                        Err(Either::Left(err)) => {
                            // Malformed requests, respond with 400
                            log::trace!("malformed request: {:?}", err);
                            let (res, body) = Response::BadRequest().finish().into_parts();
                            this.inner.error = Some(DispatchError::Parse(err));
                            *this.st = this.inner.send_response(res, body.into_body());
                        }
                        Err(Either::Right(err)) => {
                            log::trace!("peer is gone with {:?}", err);
                            // peer is gone
                            let e = DispatchError::Disconnect(Some(err));
                            set_error!(this, e);
                        }
                    }
                }
                // consume request's payload
                State::ReadPayload => {
                    if let Err(e) = ready!(this.inner.poll_request_payload(cx)) {
                        set_error!(this, e);
                    } else {
                        *this.st = this.inner.switch_to_read_request();
                    }
                }
                // send response body
                State::SendPayload { ref mut body } => {
                    if !this.inner.state.is_io_open() {
                        let e = this.inner.state.take_error().into();
                        set_error!(this, e);
                    } else if let Poll::Ready(Err(e)) = this.inner.poll_request_payload(cx)
                    {
                        set_error!(this, e);
                    } else {
                        loop {
                            ready!(this.inner.io().poll_write_backpressure(cx));
                            let item = ready!(body.poll_next_chunk(cx));
                            if let Some(st) = this.inner.send_payload(item) {
                                *this.st = st;
                                break;
                            }
                        }
                    }
                }
                // stop io tasks and call upgrade service
                State::Upgrade(ref mut req) => {
                    log::trace!("switching to upgrade service");

                    let io = this.inner.io.take().unwrap();
                    let req = req.take().unwrap();

                    // Handle UPGRADE request
                    crate::rt::spawn(this.inner.config.upgrade.as_ref().unwrap().call((
                        req,
                        io,
                        this.inner.codec.clone(),
                    )));
                    return Poll::Ready(Ok(()));
                }
                // prepare to shutdown
                State::Stop => {
                    if this
                        .inner
                        .io
                        .as_ref()
                        .unwrap()
                        .poll_shutdown(cx)?
                        .is_ready()
                    {
                        // get io error
                        if this.inner.error.is_none() {
                            this.inner.error = Some(DispatchError::Disconnect(
                                this.inner.state.take_error(),
                            ));
                        }

                        return Poll::Ready(if let Some(err) = this.inner.error.take() {
                            Err(err)
                        } else {
                            Ok(())
                        });
                    } else {
                        return Poll::Pending;
                    }
                }
            }
        }
    }
}

impl<T, S, B, X, U> DispatcherInner<T, S, B, X, U>
where
    S: Service<Request>,
    S::Error: ResponseError + 'static,
    S::Response: Into<Response<B>>,
    B: MessageBody,
{
    fn io(&self) -> &Io<T> {
        self.io.as_ref().unwrap()
    }

    fn switch_to_read_request(&mut self) -> State<B> {
        // connection is not keep-alive, disconnect
        if !self.flags.contains(Flags::KEEPALIVE) || !self.codec.keepalive_enabled() {
            self.unregister_keepalive();
            self.state.stop_dispatcher();
            State::Stop
        } else {
            self.reset_keepalive();
            State::ReadRequest
        }
    }

    fn unregister_keepalive(&mut self) {
        if self.flags.contains(Flags::KEEPALIVE) {
            self.config.timer_h1.unregister(self.expire, &self.state);
        }
    }

    fn reset_keepalive(&mut self) {
        // re-register keep-alive
        if self.flags.contains(Flags::KEEPALIVE) && self.config.keep_alive.non_zero() {
            let expire = now() + time::Duration::from(self.config.keep_alive);
            if expire != self.expire {
                self.config
                    .timer_h1
                    .register(expire, self.expire, &self.state);
                self.expire = expire;
                self.io().reset_keepalive();
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
        trace!("sending response: {:?} body: {:?}", msg, body.size());
        // we dont need to process responses if socket is disconnected
        // but we still want to handle requests with app service
        // so we skip response processing for droppped connection
        if self.state.is_io_open() {
            let result = self
                .io()
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
                trace!("got response chunk: {:?}", item.len());
                match self.io().encode(Message::Chunk(Some(item)), &self.codec) {
                    Ok(_) => None,
                    Err(err) => {
                        self.error = Some(DispatchError::Encode(err));
                        Some(State::Stop)
                    }
                }
            }
            None => {
                trace!("response payload eof");
                if let Err(err) = self.io().encode(Message::Chunk(None), &self.codec) {
                    self.error = Some(DispatchError::Encode(err));
                    Some(State::Stop)
                } else if self.flags.contains(Flags::SENDPAYLOAD_AND_STOP) {
                    Some(State::Stop)
                } else if self.payload.is_some() {
                    Some(State::ReadPayload)
                } else {
                    self.reset_keepalive();
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
        // check if payload data is required
        if let Some(ref mut payload) = self.payload {
            match payload.1.poll_data_required(cx) {
                PayloadStatus::Read => {
                    let io = self.io.as_ref().unwrap();

                    // read request payload
                    let mut updated = false;
                    loop {
                        let res = io.poll_recv(&payload.0, cx);
                        match res {
                            Poll::Ready(Ok(Some(PayloadItem::Chunk(chunk)))) => {
                                updated = true;
                                payload.1.feed_data(chunk);
                            }
                            Poll::Ready(Ok(Some(PayloadItem::Eof))) => {
                                updated = true;
                                payload.1.feed_eof();
                                self.payload = None;
                                break;
                            }
                            Poll::Ready(Ok(None)) => {
                                payload.1.set_error(PayloadError::EncodingCorrupted);
                                self.payload = None;
                                return Poll::Ready(Err(ParseError::Incomplete.into()));
                            }
                            Poll::Ready(Err(e)) => {
                                payload.1.set_error(PayloadError::EncodingCorrupted);
                                self.payload = None;
                                return Poll::Ready(Err(match e {
                                    Either::Left(e) => DispatchError::Parse(e),
                                    Either::Right(e) => DispatchError::Disconnect(Some(e)),
                                }));
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
                    self.payload = None;
                    Poll::Ready(Err(DispatchError::PayloadIsNotConsumed))
                }
            }
        } else {
            Poll::Ready(Ok(()))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::{cell::Cell, io, sync::Arc};

    use rand::Rng;

    use super::*;
    use crate::http::config::{DispatcherConfig, ServiceConfig};
    use crate::http::h1::{ClientCodec, ExpectHandler, UpgradeHandler};
    use crate::http::{body, Request, ResponseHead, StatusCode};
    use crate::io::{self as nio, Base};
    use crate::service::{boxed, fn_service, IntoService};
    use crate::util::{lazy, next, Bytes, BytesMut};
    use crate::{codec::Decoder, testing::Io, time::sleep, time::Millis, time::Seconds};

    const BUFFER_SIZE: usize = 32_768;

    /// Create http/1 dispatcher.
    pub(crate) fn h1<F, S, B>(
        stream: Io,
        service: F,
    ) -> Dispatcher<Base, S, B, ExpectHandler, UpgradeHandler<Base>>
    where
        F: IntoService<S>,
        S: Service<Request = Request>,
        S::Error: ResponseError + 'static,
        S::Response: Into<Response<B>>,
        B: MessageBody,
    {
        let config = ServiceConfig::new(
            Seconds(5).into(),
            Millis(1_000),
            Seconds::ZERO,
            Millis(5_000),
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
        F: IntoService<S>,
        S: Service<Request = Request> + 'static,
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
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_ready());
        sleep(Millis(50)).await;

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
        sleep(Millis(50)).await;
        // required because io shutdown is async oper
        let _ = lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_ready();
        sleep(Millis(50)).await;

        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_ready());
        assert!(h1.inner.state.is_closed());
        sleep(Millis(50)).await;

        client.local_buffer(|buf| assert_eq!(&buf[..26], b"HTTP/1.1 400 Bad Request\r\n"));

        client.close().await;
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_ready());
        assert!(!h1.inner.state.is_io_open());
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

        let mut buf = client.read().await.unwrap();
        assert!(load(&mut decoder, &mut buf).status.is_success());
        assert!(!client.is_server_dropped());

        client.write("GET /test2 HTTP/1.1\r\n\r\n");
        client.write("GET /test3 HTTP/1.1\r\n\r\n");

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

        client.write("GET /test1 HTTP/1.1\r\ncontent-length: 5\r\n\r\n");
        sleep(Millis(50)).await;
        client.write("xxxxx");

        let mut buf = client.read().await.unwrap();
        assert!(load(&mut decoder, &mut buf).status.is_success());
        assert!(!client.is_server_dropped());

        client.write("GET /test2 HTTP/1.1\r\n\r\n");

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
            sleep(Millis(100)).await;
            Ok::<_, io::Error>(Response::Ok().finish())
        });

        client.write("GET /test HTTP/1.1\r\n\r\n");

        let mut buf = client.read().await.unwrap();
        assert!(load(&mut decoder, &mut buf).status.is_success());
        assert!(!client.is_server_dropped());

        client.write("GET /test HTTP/1.1\r\n\r\n");
        client.write("GET /test HTTP/1.1\r\n\r\n");
        sleep(Millis(50)).await;
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
        crate::util::PoolId::P0
            .set_read_params(15 * 1024, 1024)
            .set_write_params(15 * 1024, 1024);
        h1.inner
            .io
            .as_ref()
            .unwrap()
            .set_memory_pool(crate::util::PoolId::P0.pool_ref());

        let mut decoder = ClientCodec::default();

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
        // required because io shutdown is async oper
        let _ = lazy(|cx| Pin::new(&mut h1).poll(cx)).await;
        sleep(Millis(50)).await;
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_ready());
        assert!(h1.inner.state.is_closed());

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
        let state = h1.inner.state.clone();

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
        let mut buf = client.read().await.unwrap();
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
        sleep(Millis(50)).await;
        // required because io shutdown is async oper
        let _ = lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_ready();
        sleep(Millis(50)).await;

        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_ready());
        sleep(Millis(50)).await;
        assert!(!h1.inner.state.is_io_open());
        let buf = client.local_buffer(|buf| buf.split().freeze());
        assert_eq!(&buf[..28], b"HTTP/1.1 500 Internal Server");
        assert_eq!(&buf[buf.len() - 5..], b"error");
    }
}
