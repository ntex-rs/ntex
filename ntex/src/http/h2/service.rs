use std::{cell::RefCell, io, task::Context, task::Poll};
use std::{marker::PhantomData, mem, rc::Rc};

use ntex_h2::{self as h2, frame::StreamId, server};

use crate::http::body::{BodySize, MessageBody};
use crate::http::config::{DispatcherConfig, ServiceConfig};
use crate::http::error::{DispatchError, H2Error, ResponseError};
use crate::http::header::{self, HeaderMap, HeaderName, HeaderValue};
use crate::http::message::{CurrentIo, ResponseHead};
use crate::http::{DateService, Method, Request, Response, StatusCode, Uri, Version};
use crate::io::{types, Filter, Io, IoBoxed, IoRef};
use crate::service::{IntoServiceFactory, Service, ServiceCtx, ServiceFactory};
use crate::util::{poll_fn, BoxFuture, Bytes, BytesMut, Either, HashMap, Ready};

use super::payload::{Payload, PayloadSender};

/// `ServiceFactory` implementation for HTTP2 transport
pub struct H2Service<F, S, B> {
    srv: S,
    cfg: ServiceConfig,
    _t: PhantomData<(F, B)>,
}

impl<F, S, B> H2Service<F, S, B>
where
    S: ServiceFactory<Request>,
    S::Error: ResponseError,
    S::Response: Into<Response<B>>,
    B: MessageBody,
{
    /// Create new `HttpService` instance with config.
    pub(crate) fn with_config<U: IntoServiceFactory<S, Request>>(
        cfg: ServiceConfig,
        service: U,
    ) -> Self {
        H2Service {
            cfg,
            srv: service.into_factory(),
            _t: PhantomData,
        }
    }
}

#[cfg(feature = "openssl")]
mod openssl {
    use ntex_tls::openssl::{Acceptor, SslFilter};
    use tls_openssl::ssl::SslAcceptor;

    use crate::{io::Layer, server::SslError};

    use super::*;

    impl<F, S, B> H2Service<Layer<SslFilter, F>, S, B>
    where
        F: Filter,
        S: ServiceFactory<Request> + 'static,
        S::Error: ResponseError,
        S::Response: Into<Response<B>>,
        B: MessageBody,
    {
        /// Create ssl based service
        pub fn openssl(
            self,
            acceptor: SslAcceptor,
        ) -> impl ServiceFactory<
            Io<F>,
            Response = (),
            Error = SslError<DispatchError>,
            InitError = S::InitError,
        > {
            Acceptor::new(acceptor)
                .timeout(self.cfg.0.ssl_handshake_timeout)
                .chain()
                .map_err(SslError::Ssl)
                .map_init_err(|_| panic!())
                .and_then(self.chain().map_err(SslError::Service))
        }
    }
}

#[cfg(feature = "rustls")]
mod rustls {
    use ntex_tls::rustls::{Acceptor, TlsFilter};
    use tls_rustls::ServerConfig;

    use super::*;
    use crate::{io::Layer, server::SslError};

    impl<F, S, B> H2Service<Layer<TlsFilter, F>, S, B>
    where
        F: Filter,
        S: ServiceFactory<Request> + 'static,
        S::Error: ResponseError,
        S::Response: Into<Response<B>>,
        B: MessageBody,
    {
        /// Create openssl based service
        pub fn rustls(
            self,
            mut config: ServerConfig,
        ) -> impl ServiceFactory<
            Io<F>,
            Response = (),
            Error = SslError<DispatchError>,
            InitError = S::InitError,
        > {
            let protos = vec!["h2".to_string().into()];
            config.alpn_protocols = protos;

            Acceptor::from(config)
                .timeout(self.cfg.0.ssl_handshake_timeout)
                .chain()
                .map_err(|e| SslError::Ssl(Box::new(e)))
                .map_init_err(|_| panic!())
                .and_then(self.chain().map_err(SslError::Service))
        }
    }
}

impl<F, S, B> ServiceFactory<Io<F>> for H2Service<F, S, B>
where
    F: Filter,
    S: ServiceFactory<Request> + 'static,
    S::Error: ResponseError,
    S::Response: Into<Response<B>>,
    B: MessageBody,
{
    type Response = ();
    type Error = DispatchError;
    type InitError = S::InitError;
    type Service = H2ServiceHandler<F, S::Service, B>;
    type Future<'f> = BoxFuture<'f, Result<Self::Service, Self::InitError>>;

    fn create(&self, _: ()) -> Self::Future<'_> {
        let fut = self.srv.create(());
        let cfg = self.cfg.clone();

        Box::pin(async move {
            let service = fut.await?;
            let config = Rc::new(DispatcherConfig::new(cfg, service, (), None, None));

            Ok(H2ServiceHandler {
                config,
                _t: PhantomData,
            })
        })
    }
}

/// `Service` implementation for http/2 transport
pub struct H2ServiceHandler<F, S: Service<Request>, B> {
    config: Rc<DispatcherConfig<S, (), ()>>,
    _t: PhantomData<(F, B)>,
}

impl<F, S, B> Service<Io<F>> for H2ServiceHandler<F, S, B>
where
    F: Filter,
    S: Service<Request> + 'static,
    S::Error: ResponseError,
    S::Response: Into<Response<B>>,
    B: MessageBody,
{
    type Response = ();
    type Error = DispatchError;
    type Future<'f> = BoxFuture<'f, Result<Self::Response, Self::Error>>;

    #[inline]
    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.config.service.poll_ready(cx).map_err(|e| {
            log::error!("Service readiness error: {:?}", e);
            DispatchError::Service(Box::new(e))
        })
    }

    #[inline]
    fn poll_shutdown(&self, cx: &mut Context<'_>) -> Poll<()> {
        self.config.service.poll_shutdown(cx)
    }

    fn call<'a>(&'a self, io: Io<F>, _: ServiceCtx<'a, Self>) -> Self::Future<'_> {
        log::trace!(
            "New http2 connection, peer address {:?}",
            io.query::<types::PeerAddr>().get()
        );

        Box::pin(handle(io.into(), self.config.clone()))
    }
}

pub(in crate::http) async fn handle<S, B, X, U>(
    io: IoBoxed,
    config: Rc<DispatcherConfig<S, X, U>>,
) -> Result<(), DispatchError>
where
    S: Service<Request> + 'static,
    S::Error: ResponseError,
    S::Response: Into<Response<B>>,
    B: MessageBody,
    X: 'static,
    U: 'static,
{
    io.set_disconnect_timeout(config.client_disconnect.into());
    let ioref = io.get_ref();

    let _ = server::handle_one(
        io,
        config.h2config.clone(),
        ControlService::new(),
        PublishService::new(ioref, config),
    )
    .await;

    Ok(())
}

struct ControlService {}

impl ControlService {
    fn new() -> Self {
        Self {}
    }
}

impl Service<h2::ControlMessage<H2Error>> for ControlService {
    type Response = h2::ControlResult;
    type Error = ();
    type Future<'f> = Ready<Self::Response, Self::Error>;

    fn call<'a>(
        &'a self,
        msg: h2::ControlMessage<H2Error>,
        _: ServiceCtx<'a, Self>,
    ) -> Self::Future<'a> {
        log::trace!("Control message: {:?}", msg);
        Ready::Ok::<_, ()>(msg.ack())
    }
}

struct PublishService<S: Service<Request>, B, X, U> {
    io: IoRef,
    config: Rc<DispatcherConfig<S, X, U>>,
    streams: RefCell<HashMap<StreamId, PayloadSender>>,
    _t: PhantomData<B>,
}

impl<S, B, X, U> PublishService<S, B, X, U>
where
    S: Service<Request> + 'static,
    S::Error: ResponseError,
    S::Response: Into<Response<B>>,
    B: MessageBody,
{
    fn new(io: IoRef, config: Rc<DispatcherConfig<S, X, U>>) -> Self {
        Self {
            io,
            config,
            streams: RefCell::new(HashMap::default()),
            _t: PhantomData,
        }
    }
}

impl<S, B, X, U> Service<h2::Message> for PublishService<S, B, X, U>
where
    S: Service<Request> + 'static,
    S::Error: ResponseError,
    S::Response: Into<Response<B>>,
    B: MessageBody,
    X: 'static,
    U: 'static,
{
    type Response = ();
    type Error = H2Error;
    type Future<'f> = Either<
        BoxFuture<'f, Result<Self::Response, Self::Error>>,
        Ready<Self::Response, Self::Error>,
    >;

    fn call<'a>(&'a self, msg: h2::Message, _: ServiceCtx<'a, Self>) -> Self::Future<'a> {
        let h2::Message { stream, kind } = msg;
        let (io, pseudo, headers, eof, payload) = match kind {
            h2::MessageKind::Headers {
                pseudo,
                headers,
                eof,
            } => {
                let pl = if !eof {
                    log::debug!("Creating local payload stream for {:?}", stream.id());
                    let (sender, payload) = Payload::create(stream.empty_capacity());
                    self.streams.borrow_mut().insert(stream.id(), sender);
                    Some(payload)
                } else {
                    None
                };
                (self.io.clone(), pseudo, headers, eof, pl)
            }
            h2::MessageKind::Data(data, cap) => {
                log::debug!("Got data chunk for {:?}: {:?}", stream.id(), data.len());
                if let Some(sender) = self.streams.borrow_mut().get_mut(&stream.id()) {
                    sender.feed_data(data, cap)
                } else {
                    log::error!("Payload stream does not exists for {:?}", stream.id());
                };
                return Either::Right(Ready::Ok(()));
            }
            h2::MessageKind::Eof(item) => {
                log::debug!("Got payload eof for {:?}: {:?}", stream.id(), item);
                if let Some(mut sender) = self.streams.borrow_mut().remove(&stream.id()) {
                    match item {
                        h2::StreamEof::Data(data) => {
                            sender.feed_eof(data);
                        }
                        h2::StreamEof::Trailers(_) => {
                            sender.feed_eof(Bytes::new());
                        }
                        h2::StreamEof::Error(err) => sender.set_error(err.into()),
                    }
                }
                return Either::Right(Ready::Ok(()));
            }
            h2::MessageKind::Disconnect(err) => {
                log::debug!("Connection is disconnected {:?}", err);
                if let Some(mut sender) = self.streams.borrow_mut().remove(&stream.id()) {
                    sender.set_error(io::Error::new(io::ErrorKind::Other, err).into());
                }
                return Either::Right(Ready::Ok(()));
            }
        };

        let cfg = self.config.clone();

        Either::Left(Box::pin(async move {
            log::trace!(
                "{:?} got request (eof: {}): {:#?}\nheaders: {:#?}",
                stream.id(),
                eof,
                pseudo,
                headers
            );
            let mut req = if let Some(pl) = payload {
                Request::with_payload(crate::http::Payload::H2(pl))
            } else {
                Request::new()
            };

            let path = pseudo.path.ok_or(H2Error::MissingPseudo("Path"))?;
            let method = pseudo.method.ok_or(H2Error::MissingPseudo("Method"))?;

            let head = req.head_mut();
            head.uri = if let Some(ref authority) = pseudo.authority {
                let scheme = pseudo.scheme.ok_or(H2Error::MissingPseudo("Scheme"))?;
                Uri::try_from(format!("{}://{}{}", scheme, authority, path))?
            } else {
                Uri::try_from(path.as_str())?
            };
            let is_head_req = method == Method::HEAD;
            head.version = Version::HTTP_2;
            head.method = method;
            head.headers = headers;
            head.io = CurrentIo::Ref(io);

            let (mut res, mut body) = match cfg.service.call(req).await {
                Ok(res) => res.into().into_parts(),
                Err(err) => {
                    let (res, body) = Response::from(&err).into_parts();
                    (res, body.into_body())
                }
            };

            let head = res.head_mut();
            let mut size = body.size();
            prepare_response(&cfg.timer, head, &mut size);

            log::debug!("Received service response: {:?} payload: {:?}", head, size);

            let hdrs = mem::replace(&mut head.headers, HeaderMap::new());
            if size.is_eof() || is_head_req {
                stream.send_response(head.status, hdrs, true)?;
            } else {
                stream.send_response(head.status, hdrs, false)?;

                loop {
                    match poll_fn(|cx| body.poll_next_chunk(cx)).await {
                        None => {
                            log::debug!("{:?} closing payload stream", stream.id());
                            stream.send_payload(Bytes::new(), true).await?;
                            break;
                        }
                        Some(Ok(chunk)) => {
                            log::debug!(
                                "{:?} sending data chunk {:?} bytes",
                                stream.id(),
                                chunk.len()
                            );
                            if !chunk.is_empty() {
                                stream.send_payload(chunk, false).await?;
                            }
                        }
                        Some(Err(e)) => {
                            error!("Response payload stream error: {:?}", e);
                            return Err(e.into());
                        }
                    }
                }
            }
            Ok(())
        }))
    }
}

#[allow(clippy::declare_interior_mutable_const)]
const ZERO_CONTENT_LENGTH: HeaderValue = HeaderValue::from_static("0");
#[allow(clippy::declare_interior_mutable_const)]
const KEEP_ALIVE: HeaderName = HeaderName::from_static("keep-alive");
#[allow(clippy::declare_interior_mutable_const)]
const PROXY_CONNECTION: HeaderName = HeaderName::from_static("proxy-connection");

fn prepare_response(timer: &DateService, head: &mut ResponseHead, size: &mut BodySize) {
    // Content length
    match head.status {
        StatusCode::NO_CONTENT | StatusCode::CONTINUE | StatusCode::PROCESSING => {
            *size = BodySize::None
        }
        StatusCode::SWITCHING_PROTOCOLS => {
            *size = BodySize::Stream;
        }
        _ => (),
    }
    match size {
        BodySize::None | BodySize::Stream => head.headers.remove(header::CONTENT_LENGTH),
        BodySize::Empty => head
            .headers
            .insert(header::CONTENT_LENGTH, ZERO_CONTENT_LENGTH),
        BodySize::Sized(len) => {
            head.headers.insert(
                header::CONTENT_LENGTH,
                HeaderValue::try_from(format!("{}", len)).unwrap(),
            );
        }
    };

    // http2 specific1
    head.headers.remove(header::CONNECTION);
    head.headers.remove(header::TRANSFER_ENCODING);
    head.headers.remove(header::UPGRADE);

    // omit HTTP/1.x only headers according to:
    // https://datatracker.ietf.org/doc/html/rfc7540#section-8.1.2.2
    head.headers.remove(KEEP_ALIVE);
    head.headers.remove(PROXY_CONNECTION);

    // set date header
    if !head.headers.contains_key(header::DATE) {
        let mut bytes = BytesMut::with_capacity(29);
        timer.set_date(|date| bytes.extend_from_slice(date));
        head.headers.insert(header::DATE, unsafe {
            HeaderValue::from_shared_unchecked(bytes.freeze())
        });
    }
}
