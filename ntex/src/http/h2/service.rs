use std::cell::{Cell, RefCell};
use std::{error::Error, fmt, future::poll_fn, io, marker, mem, rc::Rc, task::Context};

use ntex_h2::{self as h2, frame::StreamId, server};

use crate::channel::oneshot;
use crate::http::body::{BodySize, MessageBody};
use crate::http::config::DispatcherConfig;
use crate::http::error::{DispatchError, H2Error, ResponseError};
use crate::http::header::{self, HeaderMap, HeaderName, HeaderValue};
use crate::http::message::{CurrentIo, ResponseHead};
use crate::http::{DateService, Method, Request, Response, StatusCode, Uri, Version};
use crate::io::{Filter, Io, IoBoxed, IoRef, types};
use crate::service::{
    IntoServiceFactory, Service, ServiceCtx, ServiceFactory, cfg::SharedCfg,
};
use crate::util::{Bytes, BytesMut, HashMap, HashSet};

use super::{DefaultControlService, payload::Payload, payload::PayloadSender};

/// `ServiceFactory` implementation for HTTP2 transport
pub struct H2Service<F, S, B, C> {
    srv: S,
    ctl: Rc<C>,
    _t: marker::PhantomData<(F, B)>,
}

impl<F, S, B> H2Service<F, S, B, DefaultControlService>
where
    S: ServiceFactory<Request, SharedCfg>,
    S::Error: ResponseError,
    S::Response: Into<Response<B>>,
    B: MessageBody,
{
    /// Create new `HttpService` instance with config.
    pub(crate) fn new<U: IntoServiceFactory<S, Request, SharedCfg>>(service: U) -> Self {
        H2Service {
            srv: service.into_factory(),
            ctl: Rc::new(DefaultControlService),
            _t: marker::PhantomData,
        }
    }
}

#[cfg(feature = "openssl")]
#[allow(clippy::wildcard_imports)]
mod openssl {
    use ntex_tls::openssl::{SslAcceptor, SslFilter};
    use tls_openssl::ssl;

    use crate::{io::Layer, server::SslError};

    use super::*;

    impl<F, S, B, C> H2Service<Layer<SslFilter, F>, S, B, C>
    where
        F: Filter,
        S: ServiceFactory<Request, SharedCfg> + 'static,
        S::Error: ResponseError,
        S::InitError: fmt::Debug,
        S::Response: Into<Response<B>>,
        B: MessageBody,
        C: ServiceFactory<h2::Control<H2Error>, SharedCfg, Response = h2::ControlAck>
            + 'static,
        C::Error: Error,
        C::InitError: fmt::Debug,
    {
        /// Create ssl based service
        pub fn openssl(
            self,
            acceptor: ssl::SslAcceptor,
        ) -> impl ServiceFactory<
            Io<F>,
            SharedCfg,
            Response = (),
            Error = SslError<DispatchError>,
            InitError = (),
        > {
            SslAcceptor::new(acceptor)
                .map_err(SslError::Ssl)
                .map_init_err(|()| panic!())
                .and_then(self.map_err(SslError::Service))
        }
    }
}

#[cfg(feature = "rustls")]
#[allow(clippy::wildcard_imports)]
mod rustls {
    use ntex_tls::rustls::{TlsAcceptor, TlsServerFilter};
    use tls_rustls::ServerConfig;

    use super::*;
    use crate::{io::Layer, server::SslError};

    impl<F, S, B, C> H2Service<Layer<TlsServerFilter, F>, S, B, C>
    where
        F: Filter,
        S: ServiceFactory<Request, SharedCfg> + 'static,
        S::Error: ResponseError,
        S::InitError: fmt::Debug,
        S::Response: Into<Response<B>>,
        B: MessageBody,
        C: ServiceFactory<h2::Control<H2Error>, SharedCfg, Response = h2::ControlAck>
            + 'static,
        C::Error: Error,
        C::InitError: fmt::Debug,
    {
        /// Create openssl based service
        pub fn rustls(
            self,
            mut config: ServerConfig,
        ) -> impl ServiceFactory<
            Io<F>,
            SharedCfg,
            Response = (),
            Error = SslError<DispatchError>,
            InitError = (),
        > {
            let protos = vec!["h2".to_string().into()];
            config.alpn_protocols = protos;

            TlsAcceptor::from(config)
                .map_err(|e| SslError::Ssl(Box::new(e)))
                .map_init_err(|()| panic!())
                .and_then(self.map_err(SslError::Service))
        }
    }
}

impl<F, S, B, C> H2Service<F, S, B, C>
where
    F: Filter,
    S: ServiceFactory<Request, SharedCfg> + 'static,
    S::Response: Into<Response<B>>,
    S::Error: ResponseError,
    S::InitError: fmt::Debug,
    B: MessageBody,
    C: ServiceFactory<h2::Control<H2Error>, SharedCfg, Response = h2::ControlAck>,
    C::Error: Error,
    C::InitError: fmt::Debug,
{
    /// Provide http/2 control service
    pub fn control<CT>(self, ctl: CT) -> H2Service<F, S, B, CT>
    where
        CT: ServiceFactory<h2::Control<H2Error>, SharedCfg, Response = h2::ControlAck>,
        CT::Error: Error,
        CT::InitError: fmt::Debug,
    {
        H2Service {
            ctl: Rc::new(ctl),
            srv: self.srv,
            _t: marker::PhantomData,
        }
    }
}

impl<F, S, B, C> ServiceFactory<Io<F>, SharedCfg> for H2Service<F, S, B, C>
where
    F: Filter,
    S: ServiceFactory<Request, SharedCfg> + 'static,
    S::Error: ResponseError,
    S::InitError: fmt::Debug,
    S::Response: Into<Response<B>>,
    B: MessageBody,
    C: ServiceFactory<h2::Control<H2Error>, SharedCfg, Response = h2::ControlAck> + 'static,
    C::Error: Error,
    C::InitError: fmt::Debug,
{
    type Response = ();
    type Error = DispatchError;
    type InitError = ();
    type Service = H2ServiceHandler<F, S::Service, B, C>;

    async fn create(&self, cfg: SharedCfg) -> Result<Self::Service, Self::InitError> {
        let service = self
            .srv
            .create(cfg.clone())
            .await
            .map_err(|e| log::error!("Cannot construct publish service: {e:?}"))?;

        let (tx, rx) = oneshot::channel();
        let config = Rc::new(DispatcherConfig::new(cfg.get(), service, ()));

        Ok(H2ServiceHandler {
            cfg,
            config,
            control: self.ctl.clone(),
            inflight: RefCell::new(HashSet::default()),
            rx: Cell::new(Some(rx)),
            tx: Cell::new(Some(tx)),
            _t: marker::PhantomData,
        })
    }
}

/// `Service` implementation for http/2 transport
pub struct H2ServiceHandler<F, S: Service<Request>, B, C> {
    cfg: SharedCfg,
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
    C: ServiceFactory<h2::Control<H2Error>, SharedCfg, Response = h2::ControlAck> + 'static,
    C::Error: Error,
    C::InitError: fmt::Debug,
{
    type Response = ();
    type Error = DispatchError;

    #[inline]
    async fn ready(&self, _: ServiceCtx<'_, Self>) -> Result<(), Self::Error> {
        self.config.service.ready().await.map_err(|e| {
            log::error!("Service readiness error: {e:?}");
            DispatchError::Service(Rc::new(e))
        })
    }

    #[inline]
    fn poll(&self, cx: &mut Context<'_>) -> Result<(), Self::Error> {
        self.config
            .service
            .poll(cx)
            .map_err(|e| DispatchError::Service(Rc::new(e)))
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
            log::trace!("Shutting down service, in-flight connections: {inflight}");
            if let Some(rx) = self.rx.take() {
                let _ = rx.await;
            }

            log::trace!("Shutting down is complected",);
        }

        self.config.service.shutdown().await;
    }

    async fn call(
        &self,
        io: Io<F>,
        _: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        let control = self.control.create(self.cfg.clone()).await.map_err(|e| {
            DispatchError::Control(crate::util::str_rc_error(format!(
                "Cannot construct control service: {e:?}"
            )))
        })?;

        let id = self.config.next_id();
        let inflight = {
            let mut inflight = self.inflight.borrow_mut();
            inflight.insert(io.get_ref());
            inflight.len()
        };
        log::trace!(
            "{}: New http2 connection {id}, peer address {:?}, inflight: {inflight}",
            io.tag(),
            io.query::<types::PeerAddr>().get()
        );

        let ioref = io.get_ref();
        let result = handle(id, io.into(), control, self.config.clone()).await;
        {
            let mut inflight = self.inflight.borrow_mut();
            inflight.remove(&ioref);

            if inflight.is_empty()
                && let Some(tx) = self.tx.take()
            {
                let _ = tx.send(());
            }
        }

        result
    }
}

pub(in crate::http) async fn handle<S, B, C1: 'static, C2>(
    id: usize,
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
    let ioref = io.get_ref();

    let _ = server::handle_one(io, PublishService::new(id, ioref, config), control).await;

    Ok(())
}

struct PublishService<S: Service<Request>, B, C> {
    id: usize,
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
    fn new(id: usize, io: IoRef, config: Rc<DispatcherConfig<S, C>>) -> Self {
        Self {
            id,
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
                let pl = if eof {
                    None
                } else {
                    log::debug!(
                        "{}: Creating local payload stream for {:?}",
                        self.io.tag(),
                        stream.id()
                    );
                    let (sender, payload) = Payload::create(stream.empty_capacity());
                    self.streams.borrow_mut().insert(stream.id(), sender);
                    Some(payload)
                };
                (self.io.clone(), pseudo, headers, eof, pl)
            }
            h2::MessageKind::Data(data, cap) => {
                log::debug!(
                    "{}: Got data chunk for {:?}: {:?}",
                    self.io.tag(),
                    stream.id(),
                    data.len()
                );
                if let Some(sender) = self.streams.borrow_mut().get_mut(&stream.id()) {
                    sender.feed_data(data, cap);
                } else {
                    log::error!(
                        "{}: Payload stream does not exists for {:?}",
                        self.io.tag(),
                        stream.id()
                    );
                }
                return Ok(());
            }
            h2::MessageKind::Eof(item) => {
                log::debug!(
                    "{}: Got payload eof for {:?}: {item:?}",
                    self.io.tag(),
                    stream.id()
                );
                if let Some(sender) = self.streams.borrow_mut().remove(&stream.id()) {
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
                log::debug!("{}: Connection is disconnected {err:?}", self.io.tag(),);
                if let Some(sender) = self.streams.borrow_mut().remove(&stream.id()) {
                    sender.set_error(
                        io::Error::new(io::ErrorKind::UnexpectedEof, err).into(),
                    );
                }
                return Ok(());
            }
        };

        let cfg = self.config.clone();

        log::trace!(
            "{}: {:?} got request (eof: {eof}): {pseudo:#?}\nheaders: {headers:#?}",
            self.io.tag(),
            stream.id()
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
            Uri::try_from(format!("{scheme}://{authority}{path}"))?
        } else {
            Uri::try_from(path.as_str())?
        };
        let is_head_req = method == Method::HEAD;
        head.version = Version::HTTP_2;
        head.method = method;
        head.headers = headers;
        head.io = CurrentIo::Ref(io);
        head.id = self.id;

        let (mut res, mut body) = match cfg.service.call(req).await {
            Ok(res) => res.into().into_parts(),
            Err(err) => {
                let (res, body) = Response::from(&err).into_parts();
                (res, body.into_body())
            }
        };

        let head = res.head_mut();
        let mut size = body.size();
        prepare_response(head, &mut size);

        log::debug!(
            "{}: Received service response: {head:?} payload: {size:?}",
            self.io.tag()
        );

        let hdrs = mem::replace(&mut head.headers, HeaderMap::new());
        if size.is_eof() || is_head_req {
            stream.send_response(head.status, hdrs, true)?;
        } else {
            stream.send_response(head.status, hdrs, false)?;

            loop {
                match poll_fn(|cx| body.poll_next_chunk(cx)).await {
                    None => {
                        log::debug!(
                            "{}: {:?} closing payload stream",
                            self.io.tag(),
                            stream.id()
                        );
                        stream.send_payload(Bytes::new(), true).await?;
                        break;
                    }
                    Some(Ok(chunk)) => {
                        log::debug!(
                            "{}: {:?} sending data chunk {:?} bytes",
                            self.io.tag(),
                            stream.id(),
                            chunk.len()
                        );
                        if !chunk.is_empty() {
                            stream.send_payload(chunk, false).await?;
                        }
                    }
                    Some(Err(e)) => {
                        log::error!(
                            "{}: Response payload stream error: {e:?}",
                            self.io.tag()
                        );
                        return Err(H2Error::Stream(e));
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

fn prepare_response(head: &mut ResponseHead, size: &mut BodySize) {
    // Content length
    match head.status {
        StatusCode::NO_CONTENT | StatusCode::CONTINUE | StatusCode::PROCESSING => {
            *size = BodySize::None;
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
                HeaderValue::try_from(format!("{len}")).unwrap(),
            );
        }
    }

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
        DateService::set_date(|date| bytes.extend_from_slice(date));
        head.headers.insert(header::DATE, unsafe {
            HeaderValue::from_shared_unchecked(bytes.freeze())
        });
    }
}
