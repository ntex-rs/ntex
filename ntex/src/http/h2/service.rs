use std::cell::{Cell, RefCell};
use std::{error::Error, fmt, future::poll_fn, io, marker, mem, rc::Rc, task::Context};

use ntex_h2::{self as h2, frame::StreamId, server};

use crate::channel::oneshot;
use crate::http::body::{BodySize, MessageBody};
use crate::http::config::{DispatcherConfig, ServiceConfig};
use crate::http::error::{DispatchError, H2Error, ResponseError};
use crate::http::header::{self, HeaderMap, HeaderName, HeaderValue};
use crate::http::message::{CurrentIo, ResponseHead};
use crate::http::{DateService, Method, Request, Response, StatusCode, Uri, Version};
use crate::io::{types, Filter, Io, IoBoxed, IoRef};
use crate::service::{IntoServiceFactory, Service, ServiceCtx, ServiceFactory};
use crate::util::{Bytes, BytesMut, HashMap, HashSet};

use super::payload::{Payload, PayloadSender};
use super::DefaultControlService;

/// `ServiceFactory` implementation for HTTP2 transport
pub struct H2Service<F, S, B, C> {
    srv: S,
    ctl: Rc<C>,
    cfg: ServiceConfig,
    _t: marker::PhantomData<(F, B)>,
}

impl<F, S, B> H2Service<F, S, B, DefaultControlService>
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
            ctl: Rc::new(DefaultControlService),
            _t: marker::PhantomData,
        }
    }
}

#[cfg(feature = "openssl")]
mod openssl {
    use ntex_tls::openssl::{SslAcceptor, SslFilter};
    use tls_openssl::ssl;

    use crate::{io::Layer, server::SslError};

    use super::*;

    impl<F, S, B, C> H2Service<Layer<SslFilter, F>, S, B, C>
    where
        F: Filter,
        S: ServiceFactory<Request> + 'static,
        S::Error: ResponseError,
        S::InitError: fmt::Debug,
        S::Response: Into<Response<B>>,
        B: MessageBody,
        C: ServiceFactory<h2::Control<H2Error>, Response = h2::ControlAck> + 'static,
        C::Error: Error,
        C::InitError: fmt::Debug,
    {
        /// Create ssl based service
        pub fn openssl(
            self,
            acceptor: ssl::SslAcceptor,
        ) -> impl ServiceFactory<
            Io<F>,
            Response = (),
            Error = SslError<DispatchError>,
            InitError = (),
        > {
            SslAcceptor::new(acceptor)
                .timeout(self.cfg.ssl_handshake_timeout)
                .map_err(SslError::Ssl)
                .map_init_err(|_| panic!())
                .and_then(self.map_err(SslError::Service))
        }
    }
}

#[cfg(feature = "rustls")]
mod rustls {
    use ntex_tls::rustls::{TlsAcceptor, TlsServerFilter};
    use tls_rustls::ServerConfig;

    use super::*;
    use crate::{io::Layer, server::SslError};

    impl<F, S, B, C> H2Service<Layer<TlsServerFilter, F>, S, B, C>
    where
        F: Filter,
        S: ServiceFactory<Request> + 'static,
        S::Error: ResponseError,
        S::InitError: fmt::Debug,
        S::Response: Into<Response<B>>,
        B: MessageBody,
        C: ServiceFactory<h2::Control<H2Error>, Response = h2::ControlAck> + 'static,
        C::Error: Error,
        C::InitError: fmt::Debug,
    {
        /// Create openssl based service
        pub fn rustls(
            self,
            mut config: ServerConfig,
        ) -> impl ServiceFactory<
            Io<F>,
            Response = (),
            Error = SslError<DispatchError>,
            InitError = (),
        > {
            let protos = vec!["h2".to_string().into()];
            config.alpn_protocols = protos;

            TlsAcceptor::from(config)
                .timeout(self.cfg.ssl_handshake_timeout)
                .map_err(|e| SslError::Ssl(Box::new(e)))
                .map_init_err(|_| panic!())
                .and_then(self.map_err(SslError::Service))
        }
    }
}

impl<F, S, B, C> H2Service<F, S, B, C>
where
    F: Filter,
    S: ServiceFactory<Request> + 'static,
    S::Response: Into<Response<B>>,
    S::Error: ResponseError,
    S::InitError: fmt::Debug,
    B: MessageBody,
    C: ServiceFactory<h2::Control<H2Error>, Response = h2::ControlAck>,
    C::Error: Error,
    C::InitError: fmt::Debug,
{
    /// Provide http/2 control service
    pub fn control<CT>(self, ctl: CT) -> H2Service<F, S, B, CT>
    where
        CT: ServiceFactory<h2::Control<H2Error>, Response = h2::ControlAck>,
        CT::Error: Error,
        CT::InitError: fmt::Debug,
    {
        H2Service {
            ctl: Rc::new(ctl),
            cfg: self.cfg,
            srv: self.srv,
            _t: marker::PhantomData,
        }
    }
}

impl<F, S, B, C> ServiceFactory<Io<F>> for H2Service<F, S, B, C>
where
    F: Filter,
    S: ServiceFactory<Request> + 'static,
    S::Error: ResponseError,
    S::InitError: fmt::Debug,
    S::Response: Into<Response<B>>,
    B: MessageBody,
    C: ServiceFactory<h2::Control<H2Error>, Response = h2::ControlAck> + 'static,
    C::Error: Error,
    C::InitError: fmt::Debug,
{
    type Response = ();
    type Error = DispatchError;
    type InitError = ();
    type Service = H2ServiceHandler<F, S::Service, B, C>;

    async fn create(&self, _: ()) -> Result<Self::Service, Self::InitError> {
        let service = self
            .srv
            .create(())
            .await
            .map_err(|e| log::error!("Cannot construct publish service: {:?}", e))?;

        let (tx, rx) = oneshot::channel();
        let config = Rc::new(DispatcherConfig::new(self.cfg.clone(), service, ()));

        Ok(H2ServiceHandler {
            config,
            control: self.ctl.clone(),
            inflight: RefCell::new(Default::default()),
            rx: Cell::new(Some(rx)),
            tx: Cell::new(Some(tx)),
            _t: marker::PhantomData,
        })
    }
}

/// `Service` implementation for http/2 transport
pub struct H2ServiceHandler<F, S: Service<Request>, B, C> {
    config: Rc<DispatcherConfig<S, ()>>,
    control: Rc<C>,
    inflight: RefCell<HashSet<IoRef>>,
    rx: Cell<Option<oneshot::Receiver<()>>>,
    tx: Cell<Option<oneshot::Sender<()>>>,
    _t: marker::PhantomData<(F, B)>,
}

impl<F, S, B, C> Service<Io<F>> for H2ServiceHandler<F, S, B, C>
where
    F: Filter,
    S: Service<Request> + 'static,
    S::Error: ResponseError,
    S::Response: Into<Response<B>>,
    B: MessageBody,
    C: ServiceFactory<h2::Control<H2Error>, Response = h2::ControlAck> + 'static,
    C::Error: Error,
    C::InitError: fmt::Debug,
{
    type Response = ();
    type Error = DispatchError;

    #[inline]
    async fn ready(&self, _: ServiceCtx<'_, Self>) -> Result<(), Self::Error> {
        self.config.service.ready().await.map_err(|e| {
            log::error!("Service readiness error: {:?}", e);
            DispatchError::Service(Box::new(e))
        })
    }

    #[inline]
    fn poll(&self, cx: &mut Context<'_>) -> Result<(), Self::Error> {
        self.config
            .service
            .poll(cx)
            .map_err(|e| DispatchError::Service(Box::new(e)))
    }

    #[inline]
    async fn shutdown(&self) {
        self.config.shutdown();

        // check inflight connections
        let inflight = {
            let inflight = self.inflight.borrow();
            for io in inflight.iter() {
                io.notify_dispatcher();
            }
            inflight.len()
        };
        if inflight != 0 {
            log::trace!("Shutting down service, in-flight connections: {}", inflight);
            if let Some(rx) = self.rx.take() {
                let _ = rx.await;
            }

            log::trace!("Shutting down is complected",);
        }

        self.config.service.shutdown().await
    }

    async fn call(
        &self,
        io: Io<F>,
        _: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        let inflight = {
            let mut inflight = self.inflight.borrow_mut();
            inflight.insert(io.get_ref());
            inflight.len()
        };

        log::trace!(
            "New http2 connection, peer address {:?}, inflight: {}",
            io.query::<types::PeerAddr>().get(),
            inflight
        );
        let control = self.control.create(()).await.map_err(|e| {
            DispatchError::Control(
                format!("Cannot construct control service: {:?}", e).into(),
            )
        })?;

        let ioref = io.get_ref();
        let result = handle(io.into(), control, self.config.clone()).await;
        {
            let mut inflight = self.inflight.borrow_mut();
            inflight.remove(&ioref);

            if inflight.len() == 0 {
                if let Some(tx) = self.tx.take() {
                    let _ = tx.send(());
                }
            }
        }

        result
    }
}

pub(in crate::http) async fn handle<S, B, C1: 'static, C2>(
    io: IoBoxed,
    control: C2,
    config: Rc<DispatcherConfig<S, C1>>,
) -> Result<(), DispatchError>
where
    S: Service<Request> + 'static,
    S::Error: ResponseError,
    S::Response: Into<Response<B>>,
    B: MessageBody,
    C2: Service<h2::Control<H2Error>, Response = h2::ControlAck> + 'static,
    C2::Error: Error,
{
    io.set_disconnect_timeout(config.client_disconnect);
    let ioref = io.get_ref();

    let _ = server::handle_one(
        io,
        config.h2config.clone(),
        control,
        PublishService::new(ioref, config),
    )
    .await;

    Ok(())
}

struct PublishService<S: Service<Request>, B, C> {
    io: IoRef,
    config: Rc<DispatcherConfig<S, C>>,
    streams: RefCell<HashMap<StreamId, PayloadSender>>,
    _t: marker::PhantomData<B>,
}

impl<S, B, C> PublishService<S, B, C>
where
    S: Service<Request> + 'static,
    S::Error: ResponseError,
    S::Response: Into<Response<B>>,
    B: MessageBody,
{
    fn new(io: IoRef, config: Rc<DispatcherConfig<S, C>>) -> Self {
        Self {
            io,
            config,
            streams: RefCell::new(HashMap::default()),
            _t: marker::PhantomData,
        }
    }
}

impl<S, B, C> Service<h2::Message> for PublishService<S, B, C>
where
    S: Service<Request> + 'static,
    S::Error: ResponseError,
    S::Response: Into<Response<B>>,
    B: MessageBody,
    C: 'static,
{
    type Response = ();
    type Error = H2Error;

    async fn call(
        &self,
        msg: h2::Message,
        _: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
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
                return Ok(());
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
                return Ok(());
            }
            h2::MessageKind::Disconnect(err) => {
                log::debug!("Connection is disconnected {:?}", err);
                if let Some(mut sender) = self.streams.borrow_mut().remove(&stream.id()) {
                    sender.set_error(io::Error::new(io::ErrorKind::Other, err).into());
                }
                return Ok(());
            }
        };

        let cfg = self.config.clone();

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
                        log::error!("Response payload stream error: {:?}", e);
                        return Err(e.into());
                    }
                }
            }
        }
        Ok(())
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
