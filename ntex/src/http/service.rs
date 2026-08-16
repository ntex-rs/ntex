use std::{error::Error, marker, rc::Rc};

use crate::io::{Filter, Io, types};
use crate::service::{IntoService, IntoServiceFactory, Pipeline, Service, ServiceFactory};
use crate::util::join;
use crate::{Ctx, FromState, ReadyCtx, SharedCfg, State};

use super::error::{DispatchError, H2Error, HttpError, ResponseError};
use super::{body::MessageBody, config::DispatcherConfig};
use super::{h1, h2, request::Request, response::Response};

/// HTTP1.1/HTTP2 transport implementation
#[derive(derive_more::Debug)]
#[debug("HttpService")]
pub struct HttpService<
    St,
    F,
    Sf: ServiceFactory<Request>,
    B,
    Ctl1: Service = h1::DefaultControlService<F, HttpError>,
    Ctl2: Service = h2::DefaultControlService,
> {
    sf: Sf,
    h1_ctl: Pipeline<Ctl1>,
    h2_ctl: Pipeline<Ctl2>,
    config: DispatcherConfig,
    _t: marker::PhantomData<(St, F, B)>,
}

impl<St, F, Sf, B> HttpService<St, F, Sf, B, h1::DefaultControlService<F, HttpError>>
where
    F: Filter,
    Sf: ServiceFactory<Request, InitCfg = SharedCfg> + 'static,
    Sf::St: State<Request> + FromState<St>,
    Sf::Res: Into<Response<B>>,
    Sf::Error: ResponseError,
    Sf::InitError: Error,
    B: MessageBody,
{
    /// Create new `HttpService` instance.
    pub fn new(
        service: impl IntoServiceFactory<Sf, Request>,
    ) -> HttpService<St, F, Sf, B, h1::DefaultControlService<F, Sf::Error>> {
        HttpService {
            sf: service.into_factory(),
            h1_ctl: Pipeline::new(h1::DefaultControlService::new()),
            h2_ctl: Pipeline::new(h2::DefaultControlService),
            config: DispatcherConfig::default(),
            _t: marker::PhantomData,
        }
    }
}

impl<St, F, Sf, B> HttpService<St, F, Sf, B>
where
    F: Filter,
    Sf: ServiceFactory<Request, InitCfg = SharedCfg> + 'static,
    Sf::St: State<Request> + FromState<St>,
    Sf::Res: Into<Response<B>>,
    Sf::Error: ResponseError,
    Sf::InitError: Error,
    B: MessageBody,
{
    /// Create *http service* for HTTP/1 protocol.
    pub fn h1(
        sf: impl IntoServiceFactory<Sf, Request>,
    ) -> h1::H1Service<St, F, Sf, B, h1::DefaultControlService<F, Sf::Error>> {
        h1::H1Service::new(sf)
    }

    /// Create *http service* for HTTP/2 protocol.
    pub fn h2(sf: impl IntoServiceFactory<Sf, Request>) -> h2::H2Service<St, F, Sf, B> {
        h2::H2Service::new(sf)
    }
}

impl<St, F, Sf, B, Ctl1, Ctl2> HttpService<St, F, Sf, B, Ctl1, Ctl2>
where
    F: Filter,
    Sf: ServiceFactory<Request, InitCfg = SharedCfg> + 'static,
    Sf::St: State<Request> + FromState<St>,
    Sf::Res: Into<Response<B>>,
    Sf::Error: ResponseError,
    Sf::InitError: Error,
    B: MessageBody,
    Ctl1: Service<Req = h1::Control<F, Sf::Error>, Res = h1::ControlAck<F>>,
    Ctl1::Error: Error,
    Ctl2: Service<Req = h2::Control<H2Error>, Res = h2::ControlAck>,
    Ctl2::Error: Error,
{
    /// Provide http/1 control service.
    pub fn h1_control<Ctl>(
        self,
        ctl: impl IntoService<Ctl>,
    ) -> HttpService<F, St, Sf, B, Ctl, Ctl2>
    where
        Ctl: Service<Req = h1::Control<F, Sf::Error>, Res = h1::ControlAck<F>>,
        Ctl::St: State<Ctl::Req>,
        Ctl::Error: Error,
    {
        HttpService {
            sf: self.sf,
            config: self.config,
            h1_ctl: Pipeline::new(ctl.into_service()),
            h2_ctl: self.h2_ctl,
            _t: marker::PhantomData,
        }
    }

    /// Provide http/1 control service.
    pub fn h2_control<Ctl>(
        self,
        ctl: impl IntoService<Ctl>,
    ) -> HttpService<F, St, Sf, B, Ctl1, Ctl>
    where
        Ctl: Service<Req = h2::Control<H2Error>, Res = h2::ControlAck>,
        Ctl::St: State<Ctl::Req>,
        Ctl::Error: Error,
    {
        HttpService {
            sf: self.sf,
            config: self.config,
            h1_ctl: self.h1_ctl,
            h2_ctl: Pipeline::new(ctl.into_service()),
            _t: marker::PhantomData,
        }
    }
}

#[cfg(feature = "openssl")]
#[allow(clippy::wildcard_imports)]
mod openssl {
    use ntex_tls::openssl::{SslAcceptor, SslFilter};
    use tls_openssl::ssl;

    use super::*;
    use crate::{io::Layer, server::SslError};

    impl<St, F, Sf, B, Ctl1, Ctl2> HttpService<St, Layer<SslFilter, F>, Sf, B, Ctl1, Ctl2>
    where
        F: Filter,
        Sf: ServiceFactory<Request, InitCfg = SharedCfg> + 'static,
        Sf::St: State<Request> + FromState<St>,
        Sf::Res: Into<Response<B>>,
        Sf::Error: ResponseError,
        Sf::InitError: Error,
        B: MessageBody,
        Ctl1: Service<
                Req = h1::Control<Layer<SslFilter, F>, Sf::Error>,
                Res = h1::ControlAck<Layer<SslFilter, F>>,
            > + 'static,
        Ctl1::Error: Error,
        Ctl2: Service<Req = h2::Control<H2Error>, Res = h2::ControlAck> + 'static,
        Ctl2::Error: Error,
    {
        /// Create openssl based service
        pub fn openssl(
            self,
            acceptor: ssl::SslAcceptor,
        ) -> impl Service<St = St, Req = Io<F>, Res = (), Error = SslError<DispatchError>>
        {
            SslAcceptor::new(acceptor)
                .map_err(SslError::Ssl)
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

    impl<St, F, Sf, B, Ctl1, Ctl2> HttpService<St, Layer<TlsServerFilter, F>, Sf, B, Ctl1, Ctl2>
    where
        F: Filter,
        Sf: ServiceFactory<Request, InitCfg = SharedCfg> + 'static,
        Sf::St: State<Request> + FromState<St>,
        Sf::Res: Into<Response<B>>,
        Sf::Error: ResponseError,
        Sf::InitError: Error,
        B: MessageBody,
        Ctl1: Service<
                Req = h1::Control<Layer<TlsServerFilter, F>, Sf::Error>,
                Res = h1::ControlAck<Layer<TlsServerFilter, F>>,
            > + 'static,
        Ctl1::Error: Error,
        Ctl2: Service<Req = h2::Control<H2Error>, Res = h2::ControlAck> + 'static,
        Ctl2::Error: Error,
    {
        /// Create openssl based service
        pub fn rustls(
            self,
            mut config: ServerConfig,
        ) -> impl Service<St = St, Req = Io<F>, Res = (), Error = SslError<DispatchError>>
        {
            let protos = vec!["h2".to_string().into(), "http/1.1".to_string().into()];
            config.alpn_protocols = protos;

            TlsAcceptor::new(std::sync::Arc::new(config))
                .map_err(|e| SslError::Ssl(Box::new(e)))
                .and_then(self.map_err(SslError::Service))
        }
    }
}

impl<St, F, Sf, B, Ctl1, Ctl2> Service for HttpService<St, F, Sf, B, Ctl1, Ctl2>
where
    F: Filter,
    Sf: ServiceFactory<Request, InitCfg = SharedCfg> + 'static,
    Sf::St: State<Request> + FromState<St>,
    Sf::Res: Into<Response<B>>,
    Sf::Error: ResponseError,
    Sf::InitError: Error,
    B: MessageBody,
    Ctl1: Service<Req = h1::Control<F, Sf::Error>, Res = h1::ControlAck<F>> + 'static,
    Ctl1::Error: Error,
    Ctl2: Service<Req = h2::Control<H2Error>, Res = h2::ControlAck> + 'static,
    Ctl2::Error: Error,
{
    type St = St;
    type Req = Io<F>;
    type Res = ();
    type Error = DispatchError;

    async fn ready(&self, _: ReadyCtx<'_, Self>) -> Result<(), Self::Error> {
        let (r1, r2) = join(self.h1_ctl.ready(), self.h2_ctl.ready()).await;
        r1.map_err(|e| {
            log::error!("Http control service readiness error: {e:?}");
            DispatchError::Control(Rc::new(e))
        })?;
        r2.map_err(|e| {
            log::error!("Http control service readiness error: {e:?}");
            DispatchError::Control(Rc::new(e))
        })?;
        Ok(())
    }

    async fn shutdown(&self) {
        // check inflight connections
        let inflight = self.config.shutdown();
        if inflight != 0 {
            log::trace!("Shutting down service, in-flight connections: {inflight}");

            self.config.wait_shutdown().await;
            log::trace!("Shutting down is complected");
        }

        // shutdown control services
        join(self.h1_ctl.shutdown(), self.h2_ctl.shutdown()).await;
    }

    async fn call(&self, io: Io<F>, ctx: Ctx<'_, Self>) -> Result<Self::Res, Self::Error> {
        let cfg = io.shared();
        let svc = self
            .sf
            .create(&cfg)
            .await
            .map_err(|e| {
                log::error!("Cannot construct handler service: {e:?}");
                DispatchError::Control(Rc::new(e))
            })
            .map(|svc| Pipeline::with(svc, ctx.st()))?;

        let id = self.config.next_id();
        let ioref = io.get_ref();
        let inflight = self.config.insert_io(&ioref);

        let result = if io.query::<types::HttpProtocol>().get()
            == Some(types::HttpProtocol::Http2)
        {
            log::trace!(
                "{}: New http2 connection {id}, peer address {:?}, in-flight: {inflight}",
                io.tag(),
                io.query::<types::PeerAddr>().get(),
            );

            h2::handle(id, io.into(), svc, self.h2_ctl.bind()).await
        } else {
            log::trace!(
                "{}: New http1 connection {id}, peer address {:?}, in-flight: {inflight}",
                io.tag(),
                io.query::<types::PeerAddr>().get(),
            );

            h1::handle_io(id, io, svc, self.h1_ctl.bind(), self.config.clone()).await
        };

        let inflight = self.config.remove_io(&ioref);
        if inflight == 0 && self.config.is_shutdown() {
            self.config.notify_shutdown()
        }
        result
    }
}
