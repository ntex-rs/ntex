use std::{cell::Cell, cell::RefCell, error, fmt, marker, rc::Rc, task::Context};

use crate::io::{types, Filter, Io, IoRef};
use crate::service::{IntoServiceFactory, Service, ServiceCtx, ServiceFactory};
use crate::{channel::oneshot, util::join, util::HashSet};

use super::body::MessageBody;
use super::builder::HttpServiceBuilder;
use super::config::{DispatcherConfig, ServiceConfig};
use super::error::{DispatchError, H2Error, ResponseError};
use super::request::Request;
use super::response::Response;
use super::{h1, h2};

/// `ServiceFactory` HTTP1.1/HTTP2 transport implementation
pub struct HttpService<
    F,
    S,
    B,
    C1 = h1::DefaultControlService,
    C2 = h2::DefaultControlService,
> {
    srv: S,
    cfg: ServiceConfig,
    h1_control: C1,
    h2_control: Rc<C2>,
    _t: marker::PhantomData<(F, B)>,
}

impl<F, S, B> HttpService<F, S, B>
where
    S: ServiceFactory<Request> + 'static,
    S::Error: ResponseError,
    S::InitError: fmt::Debug,
    S::Response: Into<Response<B>>,
    B: MessageBody,
{
    /// Create builder for `HttpService` instance.
    pub fn build() -> HttpServiceBuilder<F, S> {
        HttpServiceBuilder::new()
    }

    #[doc(hidden)]
    /// Create builder for `HttpService` instance.
    pub fn build_with_config(cfg: ServiceConfig) -> HttpServiceBuilder<F, S> {
        HttpServiceBuilder::with_config(cfg)
    }
}

impl<F, S, B> HttpService<F, S, B>
where
    F: Filter,
    S: ServiceFactory<Request> + 'static,
    S::Error: ResponseError,
    S::InitError: fmt::Debug,
    S::Response: Into<Response<B>>,
    B: MessageBody,
{
    /// Create new `HttpService` instance.
    pub fn new<U: IntoServiceFactory<S, Request>>(service: U) -> Self {
        let cfg = ServiceConfig::default();

        HttpService {
            cfg,
            srv: service.into_factory(),
            h1_control: h1::DefaultControlService,
            h2_control: Rc::new(h2::DefaultControlService),
            _t: marker::PhantomData,
        }
    }

    /// Create new `HttpService` instance with config.
    pub(crate) fn with_config<U: IntoServiceFactory<S, Request>>(
        cfg: ServiceConfig,
        service: U,
    ) -> Self {
        HttpService {
            cfg,
            srv: service.into_factory(),
            h1_control: h1::DefaultControlService,
            h2_control: Rc::new(h2::DefaultControlService),
            _t: marker::PhantomData,
        }
    }
}

impl<F, S, B, C1, C2> HttpService<F, S, B, C1, C2>
where
    F: Filter,
    S: ServiceFactory<Request> + 'static,
    S::Error: ResponseError,
    S::InitError: fmt::Debug,
    S::Response: Into<Response<B>>,
    B: MessageBody,
    C1: ServiceFactory<h1::Control<F, S::Error>, Response = h1::ControlAck>,
    C1::Error: error::Error,
    C1::InitError: fmt::Debug,
    C2: ServiceFactory<h2::Control<H2Error>, Response = h2::ControlAck>,
    C2::Error: error::Error,
    C2::InitError: fmt::Debug,
{
    /// Provide http/1 control service.
    pub fn h1_control<CT>(self, control: CT) -> HttpService<F, S, B, CT, C2>
    where
        CT: ServiceFactory<h1::Control<F, S::Error>, Response = h1::ControlAck>,
        CT::Error: error::Error,
        CT::InitError: fmt::Debug,
    {
        HttpService {
            h1_control: control,
            h2_control: self.h2_control,
            cfg: self.cfg,
            srv: self.srv,
            _t: marker::PhantomData,
        }
    }

    /// Provide http/1 control service.
    pub fn h2_control<CT>(self, control: CT) -> HttpService<F, S, B, C1, CT>
    where
        CT: ServiceFactory<h2::Control<H2Error>, Response = h2::ControlAck>,
        CT::Error: error::Error,
        CT::InitError: fmt::Debug,
    {
        HttpService {
            h1_control: self.h1_control,
            h2_control: Rc::new(control),
            cfg: self.cfg,
            srv: self.srv,
            _t: marker::PhantomData,
        }
    }
}

#[cfg(feature = "openssl")]
mod openssl {
    use ntex_tls::openssl::{SslAcceptor, SslFilter};
    use tls_openssl::ssl;

    use super::*;
    use crate::{io::Layer, server::SslError};

    impl<F, S, B, C1, C2> HttpService<Layer<SslFilter, F>, S, B, C1, C2>
    where
        F: Filter,
        S: ServiceFactory<Request> + 'static,
        S::Error: ResponseError,
        S::InitError: fmt::Debug,
        S::Response: Into<Response<B>>,
        B: MessageBody,
        C1: ServiceFactory<
                h1::Control<Layer<SslFilter, F>, S::Error>,
                Response = h1::ControlAck,
            > + 'static,
        C1::Error: error::Error,
        C1::InitError: fmt::Debug,
        C2: ServiceFactory<h2::Control<H2Error>, Response = h2::ControlAck> + 'static,
        C2::Error: error::Error,
        C2::InitError: fmt::Debug,
    {
        /// Create openssl based service
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

    impl<F, S, B, C1, C2> HttpService<Layer<TlsServerFilter, F>, S, B, C1, C2>
    where
        F: Filter,
        S: ServiceFactory<Request> + 'static,
        S::Error: ResponseError,
        S::InitError: fmt::Debug,
        S::Response: Into<Response<B>>,
        B: MessageBody,
        C1: ServiceFactory<
                h1::Control<Layer<TlsServerFilter, F>, S::Error>,
                Response = h1::ControlAck,
            > + 'static,
        C1::Error: error::Error,
        C1::InitError: fmt::Debug,
        C2: ServiceFactory<h2::Control<H2Error>, Response = h2::ControlAck> + 'static,
        C2::Error: error::Error,
        C2::InitError: fmt::Debug,
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
            let protos = vec!["h2".to_string().into(), "http/1.1".to_string().into()];
            config.alpn_protocols = protos;

            TlsAcceptor::from(config)
                .timeout(self.cfg.ssl_handshake_timeout)
                .map_err(|e| SslError::Ssl(Box::new(e)))
                .map_init_err(|_| panic!())
                .and_then(self.map_err(SslError::Service))
        }
    }
}

impl<F, S, B, C1, C2> ServiceFactory<Io<F>> for HttpService<F, S, B, C1, C2>
where
    F: Filter,
    S: ServiceFactory<Request> + 'static,
    S::Error: ResponseError,
    S::InitError: fmt::Debug,
    S::Response: Into<Response<B>>,
    B: MessageBody,
    C1: ServiceFactory<h1::Control<F, S::Error>, Response = h1::ControlAck> + 'static,
    C1::Error: error::Error,
    C1::InitError: fmt::Debug,
    C2: ServiceFactory<h2::Control<H2Error>, Response = h2::ControlAck> + 'static,
    C2::Error: error::Error,
    C2::InitError: fmt::Debug,
{
    type Response = ();
    type Error = DispatchError;
    type InitError = ();
    type Service = HttpServiceHandler<F, S::Service, B, C1::Service, C2>;

    async fn create(&self, _: ()) -> Result<Self::Service, Self::InitError> {
        let service = self
            .srv
            .create(())
            .await
            .map_err(|e| log::error!("Cannot construct publish service: {e:?}"))?;
        let control = self
            .h1_control
            .create(())
            .await
            .map_err(|e| log::error!("Cannot construct control service: {e:?}"))?;

        let (tx, rx) = oneshot::channel();
        let config = DispatcherConfig::new(self.cfg.clone(), service, control);

        Ok(HttpServiceHandler {
            config: Rc::new(config),
            h2_control: self.h2_control.clone(),
            inflight: RefCell::new(HashSet::default()),
            rx: Cell::new(Some(rx)),
            tx: Cell::new(Some(tx)),
            _t: marker::PhantomData,
        })
    }
}

/// `Service` implementation for http transport
pub struct HttpServiceHandler<F, S, B, C1, C2> {
    config: Rc<DispatcherConfig<S, C1>>,
    h2_control: Rc<C2>,
    inflight: RefCell<HashSet<IoRef>>,
    rx: Cell<Option<oneshot::Receiver<()>>>,
    tx: Cell<Option<oneshot::Sender<()>>>,
    _t: marker::PhantomData<(F, B)>,
}

impl<F, S, B, C1, C2> Service<Io<F>> for HttpServiceHandler<F, S, B, C1, C2>
where
    F: Filter,
    S: Service<Request> + 'static,
    S::Error: ResponseError,
    S::Response: Into<Response<B>>,
    B: MessageBody,
    C1: Service<h1::Control<F, S::Error>, Response = h1::ControlAck> + 'static,
    C1::Error: error::Error,
    C2: ServiceFactory<h2::Control<H2Error>, Response = h2::ControlAck> + 'static,
    C2::Error: error::Error,
    C2::InitError: fmt::Debug,
{
    type Response = ();
    type Error = DispatchError;

    async fn ready(&self, _: ServiceCtx<'_, Self>) -> Result<(), Self::Error> {
        let cfg = self.config.as_ref();

        let (ready1, ready2) = join(cfg.control.ready(), cfg.service.ready()).await;
        ready1.map_err(|e| {
            log::error!("Http control service readiness error: {e:?}");
            DispatchError::Control(Box::new(e))
        })?;
        ready2.map_err(|e| {
            log::error!("Http service readiness error: {e:?}");
            DispatchError::Service(Box::new(e))
        })
    }

    #[inline]
    fn poll(&self, cx: &mut Context<'_>) -> Result<(), Self::Error> {
        let cfg = self.config.as_ref();
        cfg.control
            .poll(cx)
            .map_err(|e| DispatchError::Control(Box::new(e)))?;
        cfg.service
            .poll(cx)
            .map_err(|e| DispatchError::Service(Box::new(e)))
    }

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

        join(
            self.config.control.shutdown(),
            self.config.service.shutdown(),
        )
        .await;
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
            "New http connection, peer address {:?}, in-flight: {inflight}",
            io.query::<types::PeerAddr>().get(),
        );
        let ioref = io.get_ref();

        let result = if io.query::<types::HttpProtocol>().get()
            == Some(types::HttpProtocol::Http2)
        {
            let control = self.h2_control.create(()).await.map_err(|e| {
                DispatchError::Control(
                    format!("Cannot construct control service: {e:?}").into(),
                )
            })?;
            h2::handle(io.into(), control, self.config.clone()).await
        } else {
            h1::Dispatcher::new(io, self.config.clone())
                .await
                .map_err(DispatchError::Control)
        };

        {
            let mut inflight = self.inflight.borrow_mut();
            inflight.remove(&ioref);

            if inflight.is_empty() {
                if let Some(tx) = self.tx.take() {
                    let _ = tx.send(());
                }
            }
        }

        result
    }
}
