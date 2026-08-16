use std::{error::Error, marker, rc::Rc};

use crate::http::config::DispatcherConfig;
use crate::http::error::{DispatchError, HttpError, ResponseError};
use crate::http::{body::MessageBody, request::Request, response::Response};
use crate::io::{Filter, Io, types};
use crate::service::{
    Ctx, FromState, IntoService, IntoServiceFactory, Pipeline, PipelineBinding, ReadyCtx,
    Service, ServiceFactory, State, cfg::SharedCfg,
};

use super::control::{Control, ControlAck, ControlResult};
use super::default::DefaultControlService;
use super::dispatcher::Dispatcher;

/// `ServiceFactory` implementation for HTTP1 transport
#[derive(derive_more::Debug)]
#[debug("H1Service")]
pub struct H1Service<
    St,
    F,
    Sf: ServiceFactory<Request>,
    B,
    Ctl: Service = DefaultControlService<F, HttpError>,
> {
    sf: Sf,
    ctl: Pipeline<Ctl>,
    config: DispatcherConfig,
    _t: marker::PhantomData<(F, St, B)>,
}

impl<St, F, Sf, B> H1Service<St, F, Sf, B>
where
    F: Filter,
    Sf: ServiceFactory<Request, InitCfg = SharedCfg> + 'static,
    Sf::Res: Into<Response<B>>,
    Sf::Error: ResponseError,
    Sf::InitError: Error,
    B: MessageBody,
{
    /// Create new `HttpService` instance with config.
    pub(crate) fn new(
        service: impl IntoServiceFactory<Sf, Request>,
    ) -> H1Service<St, F, Sf, B, DefaultControlService<F, Sf::Error>> {
        H1Service {
            sf: service.into_factory(),
            ctl: Pipeline::new(DefaultControlService::new()),
            config: DispatcherConfig::default(),
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

    impl<St, F, Sf, B, Ctl> H1Service<St, Layer<SslFilter, F>, Sf, B, Ctl>
    where
        F: Filter,
        Sf: ServiceFactory<Request, InitCfg = SharedCfg> + 'static,
        Sf::St: State<Request> + FromState<St>,
        Sf::Res: Into<Response<B>>,
        Sf::Error: ResponseError,
        Sf::InitError: Error,
        B: MessageBody,
        Ctl: Service<
                Req = Control<Layer<SslFilter, F>, Sf::Error>,
                Res = ControlAck<Layer<SslFilter, F>>,
            > + 'static,
        Ctl::Error: Error,
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

    impl<St, F, Sf, B, Ctl> H1Service<St, Layer<TlsServerFilter, F>, Sf, B, Ctl>
    where
        F: Filter,
        Sf: ServiceFactory<Request, InitCfg = SharedCfg> + 'static,
        Sf::St: State<Request> + FromState<St>,
        Sf::Res: Into<Response<B>>,
        Sf::Error: ResponseError,
        Sf::InitError: Error,
        B: MessageBody,
        Ctl: Service<
                Req = Control<Layer<TlsServerFilter, F>, Sf::Error>,
                Res = ControlAck<Layer<TlsServerFilter, F>>,
            > + 'static,
        Ctl::Error: Error,
    {
        /// Create rustls based service
        pub fn rustls(
            self,
            config: ServerConfig,
        ) -> impl Service<St = St, Req = Io<F>, Res = (), Error = SslError<DispatchError>>
        {
            TlsAcceptor::new(std::sync::Arc::new(config))
                .map_err(|e| SslError::Ssl(Box::new(e)))
                .and_then(self.map_err(SslError::Service))
        }
    }
}

impl<St, F, Sf, B, Ctl> H1Service<St, F, Sf, B, Ctl>
where
    F: Filter,
    Sf: ServiceFactory<Request, InitCfg = SharedCfg>,
    Sf::St: State<Request> + FromState<St>,
    Sf::Res: Into<Response<B>>,
    Sf::Error: ResponseError,
    Sf::InitError: Error,
    B: MessageBody,
    Ctl: Service<Req = Control<F, Sf::Error>, Res = ControlAck<F>>,
    Ctl::Error: Error,
{
    /// Provide http/1 control service
    pub fn control<U>(self, ctl: impl IntoService<U>) -> H1Service<St, F, Sf, B, U>
    where
        U: Service<Req = Control<F, Sf::Error>, Res = ControlAck<F>>,
        U::St: State<Control<F, Sf::Error>>,
        U::Error: Error,
    {
        H1Service {
            sf: self.sf,
            ctl: Pipeline::new(ctl.into_service()),
            config: self.config,
            _t: marker::PhantomData,
        }
    }
}

impl<St, F, Sf, B, Ctl> Service for H1Service<St, F, Sf, B, Ctl>
where
    F: Filter,
    Sf: ServiceFactory<Request, InitCfg = SharedCfg> + 'static,
    Sf::St: State<Request> + FromState<St>,
    Sf::Res: Into<Response<B>>,
    Sf::Error: ResponseError,
    Sf::InitError: Error,
    B: MessageBody,
    Ctl: Service<Req = Control<F, Sf::Error>, Res = ControlAck<F>> + 'static,
    Ctl::Error: Error,
{
    type St = St;
    type Req = Io<F>;
    type Res = ();
    type Error = DispatchError;

    async fn ready(&self, _: ReadyCtx<'_, Self>) -> Result<(), Self::Error> {
        self.ctl.ready().await.map_err(|e| {
            log::error!("Http control service readiness error: {e:?}");
            DispatchError::Control(Rc::new(e))
        })
    }

    async fn shutdown(&self) {
        self.config.shutdown();

        // check inflight connections
        let inflight = self.config.shutdown();
        if inflight != 0 {
            log::trace!("Shutting down service, in-flight connections: {inflight}");

            self.config.wait_shutdown().await;
            log::trace!("Shutting down is complected");
        }

        self.ctl.shutdown().await;
    }

    async fn call(&self, io: Io<F>, ctx: Ctx<'_, Self>) -> Result<(), Self::Error> {
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

        log::trace!(
            "{}: New http1 connection {id}, peer address {:?}, inflight: {}",
            io.tag(),
            io.query::<types::PeerAddr>().get(),
            inflight
        );

        let result = handle_io(id, io, svc, self.ctl.bind(), self.config.clone()).await;

        let inflight = self.config.remove_io(&ioref);
        if inflight == 0 && self.config.is_shutdown() {
            self.config.notify_shutdown()
        }
        result
    }
}

pub(crate) async fn handle_io<F, S, B, Ctl>(
    id: usize,
    io: Io<F>,
    svc: Pipeline<S>,
    ctl: PipelineBinding<Ctl>,
    config: DispatcherConfig,
) -> Result<(), DispatchError>
where
    F: Filter,
    S: Service<Req = Request> + 'static,
    S::Error: ResponseError,
    S::Res: Into<Response<B>>,
    B: MessageBody,
    Ctl: Service<Req = Control<F, S::Error>, Res = ControlAck<F>> + 'static,
    Ctl::Error: Error,
{
    // Notify control service
    let ack = ctl.call_nowait(Control::connect(id, io)).await;
    match ack {
        Ok(ack) => {
            let ControlResult::Connect(io) = ack.result else {
                unreachable!();
            };

            Dispatcher::new(id, io, svc, ctl, config)
                .await
                .map_err(DispatchError::Control)
        }
        Err(e) => Err(DispatchError::Control(Rc::new(e))),
    }
}
