use std::{error::Error, fmt, marker, rc::Rc, task::Context, task::Poll};

use crate::http::body::MessageBody;
use crate::http::config::{DispatcherConfig, ServiceConfig};
use crate::http::error::{DispatchError, ResponseError};
use crate::http::{request::Request, response::Response};
use crate::io::{types, Filter, Io};
use crate::service::{IntoServiceFactory, Service, ServiceCtx, ServiceFactory};

use super::control::{Control, ControlAck};
use super::default::DefaultControlService;
use super::dispatcher::Dispatcher;

/// `ServiceFactory` implementation for HTTP1 transport
pub struct H1Service<F, S, B, C> {
    srv: S,
    ctl: C,
    cfg: ServiceConfig,
    _t: marker::PhantomData<(F, B)>,
}

impl<F, S, B> H1Service<F, S, B, DefaultControlService>
where
    S: ServiceFactory<Request> + 'static,
    S::Error: ResponseError,
    S::InitError: fmt::Debug,
    S::Response: Into<Response<B>>,
    B: MessageBody,
{
    /// Create new `HttpService` instance with config.
    pub(crate) fn with_config<U: IntoServiceFactory<S, Request>>(
        cfg: ServiceConfig,
        service: U,
    ) -> Self {
        H1Service {
            cfg,
            srv: service.into_factory(),
            ctl: DefaultControlService,
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

    impl<F, S, B, C> H1Service<Layer<SslFilter, F>, S, B, C>
    where
        F: Filter,
        S: ServiceFactory<Request> + 'static,
        S::Error: ResponseError,
        S::InitError: fmt::Debug,
        S::Response: Into<Response<B>>,
        B: MessageBody,
        C: ServiceFactory<Control<Layer<SslFilter, F>, S::Error>, Response = ControlAck>
            + 'static,
        C::Error: Error,
        C::InitError: fmt::Debug,
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
    use std::fmt;

    use ntex_tls::rustls::{TlsAcceptor, TlsServerFilter};
    use tls_rustls::ServerConfig;

    use super::*;
    use crate::{io::Layer, server::SslError};

    impl<F, S, B, C> H1Service<Layer<TlsServerFilter, F>, S, B, C>
    where
        F: Filter,
        S: ServiceFactory<Request> + 'static,
        S::Error: ResponseError,
        S::InitError: fmt::Debug,
        S::Response: Into<Response<B>>,
        B: MessageBody,
        C: ServiceFactory<
                Control<Layer<TlsServerFilter, F>, S::Error>,
                Response = ControlAck,
            > + 'static,
        C::Error: Error,
        C::InitError: fmt::Debug,
    {
        /// Create rustls based service
        pub fn rustls(
            self,
            config: ServerConfig,
        ) -> impl ServiceFactory<
            Io<F>,
            Response = (),
            Error = SslError<DispatchError>,
            InitError = (),
        > {
            TlsAcceptor::from(config)
                .timeout(self.cfg.ssl_handshake_timeout)
                .map_err(|e| SslError::Ssl(Box::new(e)))
                .map_init_err(|_| panic!())
                .and_then(self.map_err(SslError::Service))
        }
    }
}

impl<F, S, B, C> H1Service<F, S, B, C>
where
    F: Filter,
    S: ServiceFactory<Request>,
    S::Error: ResponseError,
    S::Response: Into<Response<B>>,
    S::InitError: fmt::Debug,
    B: MessageBody,
    C: ServiceFactory<Control<F, S::Error>, Response = ControlAck>,
    C::Error: Error,
    C::InitError: fmt::Debug,
{
    /// Provide http/1 control service
    pub fn control<C1>(self, ctl: C1) -> H1Service<F, S, B, C1>
    where
        C1: ServiceFactory<Control<F, S::Error>, Response = ControlAck>,
        C1::Error: Error,
        C1::InitError: fmt::Debug,
    {
        H1Service {
            ctl,
            cfg: self.cfg,
            srv: self.srv,
            _t: marker::PhantomData,
        }
    }
}

impl<F, S, B, C> ServiceFactory<Io<F>> for H1Service<F, S, B, C>
where
    F: Filter,
    S: ServiceFactory<Request> + 'static,
    S::Error: ResponseError,
    S::Response: Into<Response<B>>,
    S::InitError: fmt::Debug,
    B: MessageBody,
    C: ServiceFactory<Control<F, S::Error>, Response = ControlAck> + 'static,
    C::Error: Error,
    C::InitError: fmt::Debug,
{
    type Response = ();
    type Error = DispatchError;
    type InitError = ();
    type Service = H1ServiceHandler<F, S::Service, B, C::Service>;

    async fn create(&self, _: ()) -> Result<Self::Service, Self::InitError> {
        let service = self
            .srv
            .create(())
            .await
            .map_err(|e| log::error!("Cannot construct publish service: {:?}", e))?;
        let control = self
            .ctl
            .create(())
            .await
            .map_err(|e| log::error!("Cannot construct control service: {:?}", e))?;

        let config = Rc::new(DispatcherConfig::new(self.cfg.clone(), service, control));

        Ok(H1ServiceHandler {
            config,
            _t: marker::PhantomData,
        })
    }
}

/// `Service` implementation for HTTP1 transport
pub struct H1ServiceHandler<F, S, B, C> {
    config: Rc<DispatcherConfig<S, C>>,
    _t: marker::PhantomData<(F, B)>,
}

impl<F, S, B, C> Service<Io<F>> for H1ServiceHandler<F, S, B, C>
where
    F: Filter,
    C: Service<Control<F, S::Error>, Response = ControlAck> + 'static,
    C::Error: Error,
    S: Service<Request> + 'static,
    S::Error: ResponseError,
    S::Response: Into<Response<B>>,
    B: MessageBody,
{
    type Response = ();
    type Error = DispatchError;

    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let cfg = self.config.as_ref();

        let ready1 = cfg
            .control
            .poll_ready(cx)
            .map_err(|e| {
                log::error!("Http control service readiness error: {:?}", e);
                DispatchError::Control(Box::new(e))
            })?
            .is_ready();

        let ready2 = cfg
            .service
            .poll_ready(cx)
            .map_err(|e| {
                log::error!("Http service readiness error: {:?}", e);
                DispatchError::Service(Box::new(e))
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

    async fn call(&self, io: Io<F>, _: ServiceCtx<'_, Self>) -> Result<(), Self::Error> {
        log::trace!(
            "New http1 connection, peer address {:?}",
            io.query::<types::PeerAddr>().get()
        );

        Dispatcher::new(io, self.config.clone())
            .await
            .map_err(DispatchError::Control)
    }
}
