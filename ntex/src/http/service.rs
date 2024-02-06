use std::{error, fmt, io, marker, rc::Rc, task::Context, task::Poll};

use crate::io::{types, Filter, Io};
use crate::service::{IntoServiceFactory, Service, ServiceCtx, ServiceFactory};

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
    C1::InitError: error::Error,
    C2: ServiceFactory<h2::ControlMessage<H2Error>, Response = h2::ControlResult>,
    C2::Error: error::Error,
    C2::InitError: error::Error,
{
    /// Provide http/1 control service.
    pub fn h1_control<CT>(self, control: CT) -> HttpService<F, S, B, CT, C2>
    where
        CT: ServiceFactory<h1::Control<F, S::Error>, Response = h1::ControlAck>,
        CT::Error: error::Error,
        CT::InitError: error::Error,
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
        CT: ServiceFactory<h2::ControlMessage<H2Error>, Response = h2::ControlResult>,
        CT::Error: error::Error,
        CT::InitError: error::Error,
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
        C1::InitError: error::Error,
        C2: ServiceFactory<h2::ControlMessage<H2Error>, Response = h2::ControlResult>
            + 'static,
        C2::Error: error::Error,
        C2::InitError: error::Error,
    {
        /// Create openssl based service
        pub fn openssl(
            self,
            acceptor: ssl::SslAcceptor,
        ) -> impl ServiceFactory<
            Io<F>,
            Response = (),
            Error = SslError<DispatchError>,
            InitError = Box<dyn error::Error>,
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
        C1::InitError: error::Error,
        C2: ServiceFactory<h2::ControlMessage<H2Error>, Response = h2::ControlResult>
            + 'static,
        C2::Error: error::Error,
        C2::InitError: error::Error,
    {
        /// Create openssl based service
        pub fn rustls(
            self,
            mut config: ServerConfig,
        ) -> impl ServiceFactory<
            Io<F>,
            Response = (),
            Error = SslError<DispatchError>,
            InitError = Box<dyn error::Error>,
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
    C1::InitError: error::Error,
    C2: ServiceFactory<h2::ControlMessage<H2Error>, Response = h2::ControlResult> + 'static,
    C2::Error: error::Error,
    C2::InitError: error::Error,
{
    type Response = ();
    type Error = DispatchError;
    type InitError = Box<dyn error::Error>;
    type Service = HttpServiceHandler<F, S::Service, B, C1::Service, C2>;

    async fn create(&self, _: ()) -> Result<Self::Service, Self::InitError> {
        let service = self.srv.create(()).await.map_err(|e| {
            log::error!("Init http service error: {:?}", e);
            Box::new(io::Error::new(io::ErrorKind::Other, format!("{:?}", e)))
        })?;

        let control = self.h1_control.create(()).await.map_err(|e| {
            log::error!("Init http service error: {:?}", e);
            Box::new(e)
        })?;

        let config = DispatcherConfig::new(self.cfg.clone(), service, control);

        Ok(HttpServiceHandler {
            config: Rc::new(config),
            h2_control: self.h2_control.clone(),
            _t: marker::PhantomData,
        })
    }
}

/// `Service` implementation for http transport
pub struct HttpServiceHandler<F, S, B, C1, C2> {
    config: Rc<DispatcherConfig<S, C1>>,
    h2_control: Rc<C2>,
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
    C2: ServiceFactory<h2::ControlMessage<H2Error>, Response = h2::ControlResult> + 'static,
    C2::Error: error::Error,
    C2::InitError: error::Error,
{
    type Response = ();
    type Error = DispatchError;

    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let cfg = self.config.as_ref();

        let ready1 = cfg
            .service
            .poll_ready(cx)
            .map_err(|e| {
                log::error!("Http service readiness error: {:?}", e);
                DispatchError::Service(Box::new(e))
            })?
            .is_ready();

        let ready2 = cfg
            .control
            .poll_ready(cx)
            .map_err(|e| {
                log::error!("Http control service readiness error: {:?}", e);
                DispatchError::Control(Box::new(e))
            })?
            .is_ready();

        if ready1 && ready2 {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }

    fn poll_shutdown(&self, cx: &mut Context<'_>) -> Poll<()> {
        let ready1 = self.config.control.poll_shutdown(cx).is_ready();
        let ready2 = self.config.service.poll_shutdown(cx).is_ready();

        if ready1 && ready2 {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }

    async fn call(
        &self,
        io: Io<F>,
        _: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        log::trace!(
            "New http connection, peer address {:?}",
            io.query::<types::PeerAddr>().get()
        );

        if io.query::<types::HttpProtocol>().get() == Some(types::HttpProtocol::Http2) {
            let control = self
                .h2_control
                .create(())
                .await
                .map_err(|e| DispatchError::Control(e.into()))?;
            h2::handle(io.into(), control, self.config.clone()).await
        } else {
            h1::Dispatcher::new(io, self.config.clone())
                .await
                .map_err(DispatchError::Control)
        }
    }
}
