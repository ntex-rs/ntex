use std::task::{Context, Poll};
use std::{
    convert::TryFrom, future::Future, marker::PhantomData, net, pin::Pin, rc::Rc,
};

use bytes::{Bytes, BytesMut};
use h2::server::{Connection, SendResponse};
use h2::SendStream;
use http::header::{HeaderValue, CONNECTION, CONTENT_LENGTH, DATE, TRANSFER_ENCODING};
use log::{error, trace};

use crate::codec::{AsyncRead, AsyncWrite};
use crate::http::body::{BodySize, MessageBody, ResponseBody};
use crate::http::config::{DateService, DispatcherConfig};
use crate::http::error::{DispatchError, ResponseError};
use crate::http::helpers::DataFactory;
use crate::http::message::ResponseHead;
use crate::http::payload::Payload;
use crate::http::request::Request;
use crate::http::response::Response;
use crate::rt::time::{Instant, Sleep};
use crate::Service;

const CHUNK_SIZE: usize = 16_384;

pin_project_lite::pin_project! {
    /// Dispatcher for HTTP/2 protocol
    pub struct Dispatcher<T, S: Service<Request = Request>, B: MessageBody, X, U> {
        config: Rc<DispatcherConfig<T, S, X, U>>,
        connection: Connection<T, Bytes>,
        on_connect: Option<Box<dyn DataFactory>>,
        peer_addr: Option<net::SocketAddr>,
        ka_expire: Instant,
        ka_timer: Option<Sleep>,
        _t: PhantomData<B>,
    }
}

impl<T, S, B, X, U> Dispatcher<T, S, B, X, U>
where
    T: AsyncRead + AsyncWrite + Unpin,
    S: Service<Request = Request>,
    S::Error: ResponseError + 'static,
    S::Response: Into<Response<B>>,
    B: MessageBody,
{
    pub(in crate::http) fn new(
        config: Rc<DispatcherConfig<T, S, X, U>>,
        connection: Connection<T, Bytes>,
        on_connect: Option<Box<dyn DataFactory>>,
        timeout: Option<Sleep>,
        peer_addr: Option<net::SocketAddr>,
    ) -> Self {
        // keep-alive timer
        let (ka_expire, ka_timer) = if let Some(delay) = timeout {
            (delay.deadline(), Some(delay))
        } else if let Some(delay) = config.keep_alive_timer() {
            (delay.deadline(), Some(delay))
        } else {
            (config.now(), None)
        };

        Dispatcher {
            config,
            peer_addr,
            connection,
            on_connect,
            ka_expire,
            ka_timer,
            _t: PhantomData,
        }
    }
}

impl<T, S, B, X, U> Future for Dispatcher<T, S, B, X, U>
where
    T: AsyncRead + AsyncWrite + Unpin,
    S: Service<Request = Request>,
    S::Error: ResponseError + 'static,
    S::Future: 'static,
    S::Response: Into<Response<B>> + 'static,
    B: MessageBody + 'static,
{
    type Output = Result<(), DispatchError>;

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        loop {
            match Pin::new(&mut this.connection).poll_accept(cx) {
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Ready(Some(Err(err))) => return Poll::Ready(Err(err.into())),
                Poll::Ready(Some(Ok((req, res)))) => {
                    trace!("h2 message is received: {:?}", req);

                    // update keep-alive expire
                    if this.ka_timer.is_some() {
                        if let Some(expire) = this.config.keep_alive_expire() {
                            this.ka_expire = expire;
                        }
                    }

                    let (parts, body) = req.into_parts();
                    let mut req = Request::with_payload(Payload::H2(
                        crate::http::h2::Payload::new(body),
                    ));

                    let head = &mut req.head_mut();
                    head.uri = parts.uri;
                    head.method = parts.method;
                    head.version = parts.version;
                    head.headers = parts.headers.into();
                    head.peer_addr = this.peer_addr;

                    // set on_connect data
                    if let Some(ref on_connect) = this.on_connect {
                        on_connect.set(&mut req.extensions_mut());
                    }

                    crate::rt::spawn(ServiceResponse {
                        state: ServiceResponseState::ServiceCall {
                            call: this.config.service.call(req),
                            send: Some(res),
                        },
                        timer: this.config.timer.clone(),
                        buffer: None,
                        _t: PhantomData,
                    });
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

pin_project_lite::pin_project! {
    struct ServiceResponse<F, I, E, B> {
        #[pin]
        state: ServiceResponseState<F, B>,
        timer: DateService,
        buffer: Option<Bytes>,
        _t: PhantomData<(I, E)>,
    }
}

pin_project_lite::pin_project! {
    #[project = ServiceResponseStateProject]
    enum ServiceResponseState<F, B> {
        ServiceCall { #[pin] call: F, send: Option<SendResponse<Bytes>> },
        SendPayload { stream: SendStream<Bytes>, body: ResponseBody<B> },
    }
}

impl<F, I, E, B> ServiceResponse<F, I, E, B>
where
    F: Future<Output = Result<I, E>>,
    E: ResponseError + 'static,
    I: Into<Response<B>>,
    B: MessageBody,
{
    fn prepare_response(
        &self,
        head: &ResponseHead,
        size: &mut BodySize,
    ) -> http::Response<()> {
        let mut has_date = false;
        let mut skip_len = size != &BodySize::Stream;

        let mut res = http::Response::new(());
        *res.status_mut() = head.status;
        *res.version_mut() = http::Version::HTTP_2;

        // Content length
        match head.status {
            http::StatusCode::NO_CONTENT
            | http::StatusCode::CONTINUE
            | http::StatusCode::PROCESSING => *size = BodySize::None,
            http::StatusCode::SWITCHING_PROTOCOLS => {
                skip_len = true;
                *size = BodySize::Stream;
            }
            _ => (),
        }
        let _ = match size {
            BodySize::None | BodySize::Stream => None,
            BodySize::Empty => res
                .headers_mut()
                .insert(CONTENT_LENGTH, HeaderValue::from_static("0")),
            BodySize::Sized(len) => res.headers_mut().insert(
                CONTENT_LENGTH,
                HeaderValue::try_from(format!("{}", len)).unwrap(),
            ),
        };

        // copy headers
        for (key, value) in head.headers.iter() {
            match *key {
                CONNECTION | TRANSFER_ENCODING => continue, // http2 specific
                CONTENT_LENGTH if skip_len => continue,
                DATE => has_date = true,
                _ => (),
            }
            res.headers_mut().append(key, value.clone());
        }

        // set date header
        if !has_date {
            let mut bytes = BytesMut::with_capacity(29);
            self.timer.set_date(|date| bytes.extend_from_slice(date));
            res.headers_mut().insert(DATE, unsafe {
                HeaderValue::from_maybe_shared_unchecked(bytes.freeze())
            });
        }

        res
    }
}

impl<F, I, E, B> Future for ServiceResponse<F, I, E, B>
where
    F: Future<Output = Result<I, E>>,
    E: ResponseError + 'static,
    I: Into<Response<B>>,
    B: MessageBody,
{
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.as_mut().project();

        match this.state.project() {
            ServiceResponseStateProject::ServiceCall { call, send } => {
                match call.poll(cx) {
                    Poll::Ready(Ok(res)) => {
                        let (res, body) = res.into().replace_body(());

                        let mut send = send.take().unwrap();
                        let mut size = body.size();
                        let h2_res =
                            self.as_mut().prepare_response(res.head(), &mut size);
                        this = self.as_mut().project();

                        let stream = match send.send_response(h2_res, size.is_eof()) {
                            Err(e) => {
                                trace!("Error sending h2 response: {:?}", e);
                                return Poll::Ready(());
                            }
                            Ok(stream) => stream,
                        };

                        if size.is_eof() {
                            Poll::Ready(())
                        } else {
                            this.state
                                .set(ServiceResponseState::SendPayload { stream, body });
                            self.poll(cx)
                        }
                    }
                    Poll::Pending => Poll::Pending,
                    Poll::Ready(Err(e)) => {
                        let res: Response = (&e).into();
                        let (res, body) = res.replace_body(());

                        let mut send = send.take().unwrap();
                        let mut size = body.size();
                        let h2_res =
                            self.as_mut().prepare_response(res.head(), &mut size);
                        this = self.as_mut().project();

                        let stream = match send.send_response(h2_res, size.is_eof()) {
                            Err(e) => {
                                trace!("Error sending h2 response: {:?}", e);
                                return Poll::Ready(());
                            }
                            Ok(stream) => stream,
                        };

                        if size.is_eof() {
                            Poll::Ready(())
                        } else {
                            this.state.set(ServiceResponseState::SendPayload {
                                stream,
                                body: body.into_body(),
                            });
                            self.poll(cx)
                        }
                    }
                }
            }
            ServiceResponseStateProject::SendPayload { stream, body } => loop {
                loop {
                    if let Some(buffer) = this.buffer {
                        match stream.poll_capacity(cx) {
                            Poll::Pending => return Poll::Pending,
                            Poll::Ready(None) => return Poll::Ready(()),
                            Poll::Ready(Some(Ok(cap))) => {
                                let len = buffer.len();
                                let bytes = buffer.split_to(std::cmp::min(cap, len));

                                if let Err(e) = stream.send_data(bytes, false) {
                                    warn!("{:?}", e);
                                    return Poll::Ready(());
                                } else if !buffer.is_empty() {
                                    let cap = std::cmp::min(buffer.len(), CHUNK_SIZE);
                                    stream.reserve_capacity(cap);
                                } else {
                                    this.buffer.take();
                                }
                            }
                            Poll::Ready(Some(Err(e))) => {
                                warn!("{:?}", e);
                                return Poll::Ready(());
                            }
                        }
                    } else {
                        match body.poll_next_chunk(cx) {
                            Poll::Pending => return Poll::Pending,
                            Poll::Ready(None) => {
                                if let Err(e) = stream.send_data(Bytes::new(), true) {
                                    warn!("{:?}", e);
                                }
                                return Poll::Ready(());
                            }
                            Poll::Ready(Some(Ok(chunk))) => {
                                stream.reserve_capacity(std::cmp::min(
                                    chunk.len(),
                                    CHUNK_SIZE,
                                ));
                                *this.buffer = Some(chunk);
                            }
                            Poll::Ready(Some(Err(e))) => {
                                error!("Response payload stream error: {:?}", e);
                                return Poll::Ready(());
                            }
                        }
                    }
                }
            },
        }
    }
}
