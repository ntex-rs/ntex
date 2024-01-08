use std::{cell, error, fmt, marker, rc::Rc, task::Context, task::Poll};

use crate::io::{types, Filter, Io};
use crate::service::{IntoServiceFactory, Service, ServiceCtx, ServiceFactory};

use super::body::MessageBody;
use super::builder::HttpServiceBuilder;
use super::config::{DispatcherConfig, OnRequest, ServiceConfig};
use super::error::{DispatchError, ResponseError};
use super::request::Request;
use super::response::Response;
use super::{h1, h2};

/// `ServiceFactory` HTTP1.1/HTTP2 transport implementation
pub struct HttpService<F, S, B, X = h1::ExpectHandler, U = h1::UpgradeHandler<F>> {
    srv: S,
    cfg: ServiceConfig,
    expect: X,
    upgrade: Option<U>,
    on_request: cell::RefCell<Option<OnRequest>>,
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
            expect: h1::ExpectHandler,
            upgrade: None,
            on_request: cell::RefCell::new(None),
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
            expect: h1::ExpectHandler,
            upgrade: None,
            on_request: cell::RefCell::new(None),
            _t: marker::PhantomData,
        }
    }
}

impl<F, S, B, X, U> HttpService<F, S, B, X, U>
where
    F: Filter,
    S: ServiceFactory<Request> + 'static,
    S::Error: ResponseError,
    S::InitError: fmt::Debug,
    S::Response: Into<Response<B>>,
    B: MessageBody,
{
    /// Provide service for `EXPECT: 100-Continue` support.
    ///
    /// Service get called with request that contains `EXPECT` header.
    /// Service must return request in case of success, in that case
    /// request will be forwarded to main service.
    pub fn expect<X1>(self, expect: X1) -> HttpService<F, S, B, X1, U>
    where
        X1: ServiceFactory<Request, Response = Request>,
        X1::Error: ResponseError,
        X1::InitError: fmt::Debug,
    {
        HttpService {
            expect,
            cfg: self.cfg,
            srv: self.srv,
            upgrade: self.upgrade,
            on_request: self.on_request,
            _t: marker::PhantomData,
        }
    }

    /// Provide service for custom `Connection: UPGRADE` support.
    ///
    /// If service is provided then normal requests handling get halted
    /// and this service get called with original request and framed object.
    pub fn upgrade<U1>(self, upgrade: Option<U1>) -> HttpService<F, S, B, X, U1>
    where
        U1: ServiceFactory<(Request, Io<F>, h1::Codec), Response = ()>,
        U1::Error: fmt::Display + error::Error,
        U1::InitError: fmt::Debug,
    {
        HttpService {
            upgrade,
            cfg: self.cfg,
            srv: self.srv,
            expect: self.expect,
            on_request: self.on_request,
            _t: marker::PhantomData,
        }
    }

    /// Set on request callback.
    pub(crate) fn on_request(self, f: Option<OnRequest>) -> Self {
        *self.on_request.borrow_mut() = f;
        self
    }
}

#[cfg(feature = "openssl")]
mod openssl {
    use ntex_tls::openssl::{SslAcceptor, SslFilter};
    use tls_openssl::ssl;

    use super::*;
    use crate::{io::Layer, server::SslError};

    impl<F, S, B, X, U> HttpService<Layer<SslFilter, F>, S, B, X, U>
    where
        F: Filter,
        S: ServiceFactory<Request> + 'static,
        S::Error: ResponseError,
        S::InitError: fmt::Debug,
        S::Response: Into<Response<B>>,
        B: MessageBody,
        X: ServiceFactory<Request, Response = Request> + 'static,
        X::Error: ResponseError,
        X::InitError: fmt::Debug,
        U: ServiceFactory<(Request, Io<Layer<SslFilter, F>>, h1::Codec), Response = ()>
            + 'static,
        U::Error: fmt::Display + error::Error,
        U::InitError: fmt::Debug,
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

    impl<F, S, B, X, U> HttpService<Layer<TlsServerFilter, F>, S, B, X, U>
    where
        F: Filter,
        S: ServiceFactory<Request> + 'static,
        S::Error: ResponseError,
        S::InitError: fmt::Debug,
        S::Response: Into<Response<B>>,
        B: MessageBody,
        X: ServiceFactory<Request, Response = Request> + 'static,
        X::Error: ResponseError,
        X::InitError: fmt::Debug,
        U: ServiceFactory<
                (Request, Io<Layer<TlsServerFilter, F>>, h1::Codec),
                Response = (),
            > + 'static,
        U::Error: fmt::Display + error::Error,
        U::InitError: fmt::Debug,
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

impl<F, S, B, X, U> ServiceFactory<Io<F>> for HttpService<F, S, B, X, U>
where
    F: Filter,
    S: ServiceFactory<Request> + 'static,
    S::Error: ResponseError,
    S::InitError: fmt::Debug,
    S::Response: Into<Response<B>>,
    B: MessageBody,
    X: ServiceFactory<Request, Response = Request> + 'static,
    X::Error: ResponseError,
    X::InitError: fmt::Debug,
    U: ServiceFactory<(Request, Io<F>, h1::Codec), Response = ()> + 'static,
    U::Error: fmt::Display + error::Error,
    U::InitError: fmt::Debug,
{
    type Response = ();
    type Error = DispatchError;
    type InitError = ();
    type Service = HttpServiceHandler<F, S::Service, B, X::Service, U::Service>;

    async fn create(&self, _: ()) -> Result<Self::Service, Self::InitError> {
        let fut = self.srv.create(());
        let fut_ex = self.expect.create(());
        let fut_upg = self.upgrade.as_ref().map(|f| f.create(()));
        let on_request = self.on_request.borrow_mut().take();
        let cfg = self.cfg.clone();

        let service = fut
            .await
            .map_err(|e| log::error!("Init http service error: {:?}", e))?;

        let expect = fut_ex
            .await
            .map_err(|e| log::error!("Init http service error: {:?}", e))?;

        let upgrade = if let Some(fut) = fut_upg {
            Some(
                fut.await
                    .map_err(|e| log::error!("Init http service error: {:?}", e))?,
            )
        } else {
            None
        };

        let config = DispatcherConfig::new(cfg, service, expect, upgrade, on_request);

        Ok(HttpServiceHandler {
            config: Rc::new(config),
            _t: marker::PhantomData,
        })
    }
}

/// `Service` implementation for http transport
pub struct HttpServiceHandler<F, S, B, X, U> {
    config: Rc<DispatcherConfig<S, X, U>>,
    _t: marker::PhantomData<(F, B)>,
}

impl<F, S, B, X, U> Service<Io<F>> for HttpServiceHandler<F, S, B, X, U>
where
    F: Filter,
    S: Service<Request> + 'static,
    S::Error: ResponseError,
    S::Response: Into<Response<B>>,
    B: MessageBody,
    X: Service<Request, Response = Request> + 'static,
    X::Error: ResponseError,
    U: Service<(Request, Io<F>, h1::Codec), Response = ()> + 'static,
    U::Error: fmt::Display + error::Error,
{
    type Response = ();
    type Error = DispatchError;

    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let cfg = self.config.as_ref();

        let ready = cfg
            .expect
            .poll_ready(cx)
            .map_err(|e| {
                log::error!("Http service readiness error: {:?}", e);
                DispatchError::Service(Box::new(e))
            })?
            .is_ready();

        let ready = cfg
            .service
            .poll_ready(cx)
            .map_err(|e| {
                log::error!("Http service readiness error: {:?}", e);
                DispatchError::Service(Box::new(e))
            })?
            .is_ready()
            && ready;

        let ready = if let Some(ref upg) = cfg.upgrade {
            upg.poll_ready(cx)
                .map_err(|e| {
                    log::error!("Http service readiness error: {:?}", e);
                    DispatchError::Upgrade(Box::new(e))
                })?
                .is_ready()
                && ready
        } else {
            ready
        };

        if ready {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }

    fn poll_shutdown(&self, cx: &mut Context<'_>) -> Poll<()> {
        let ready = self.config.expect.poll_shutdown(cx).is_ready();
        let ready = self.config.service.poll_shutdown(cx).is_ready() && ready;
        let ready = if let Some(ref upg) = self.config.upgrade {
            upg.poll_shutdown(cx).is_ready() && ready
        } else {
            ready
        };

        if ready {
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
            h2::handle(io.into(), self.config.clone()).await
        } else {
            h1::Dispatcher::new(io, self.config.clone()).await
        }
    }
}
