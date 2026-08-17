use std::{error::Error, marker, rc::Rc};

use crate::http::config::DispatcherConfig;
use crate::http::error::{DispatchError, ResponseError};
use crate::http::{HttpPipeline, body::MessageBody, request::Request, response::Response};
use crate::io::{Filter, Io, types};
use crate::service::{
    Ctx, FromState, IntoService, IntoServiceFactory, Pipeline, PipelineBinding, PipelineFactory,
    ReadyCtx, Service, ServiceFactory, State, cfg::SharedCfg,
};
use crate::util::dyn_rc_err;

use super::control::{Control, ControlAck, ControlResult};
use super::default::DefaultControlService;
use super::dispatcher::Dispatcher;

/// `ServiceFactory` implementation for HTTP1 transport
#[derive(derive_more::Debug)]
#[debug("H1Service")]
pub struct H1Service<Hst, F, B, Err> {
    sf: HttpPipeline<Hst, B, Err>,
    ctl: Pipeline<Control<F, Err>, ControlAck<F>, Rc<dyn Error>>,
    config: DispatcherConfig,
    _t: marker::PhantomData<(Hst, F, B)>,
}

impl<Hst, F, B, Err> H1Service<Hst, F, B, Err>
where
    F: Filter,
    B: MessageBody,
    Err: ResponseError + 'static,
{
    /// Create new `HttpService` instance with config.
    pub(crate) fn new<Sf, St>(
        service: impl IntoServiceFactory<Sf, St, Request>,
    ) -> H1Service<Hst, F, B, Err>
    where
        Hst: 'static,
        Sf: ServiceFactory<Request, St, Error = Err, InitCfg = SharedCfg> + 'static,
        Sf::Res: Into<Response<B>>,
        Sf::InitError: Error,
        St: State<Request> + FromState<Hst>,
    {
        H1Service {
            sf: PipelineFactory::new(
                service
                    .into_factory()
                    .map(|res| res.into())
                    .map_init_err(dyn_rc_err),
            ),
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

    impl<Hst, F, B, Err> H1Service<Hst, Layer<SslFilter, F>, B, Err>
    where
        F: Filter,
        B: MessageBody,
        Err: ResponseError + 'static,
    {
        /// Create openssl based service
        pub fn openssl(
            self,
            acceptor: ssl::SslAcceptor,
        ) -> impl Service<Hst, Req = Io<F>, Res = (), Error = SslError<DispatchError>> {
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

    impl<Hst, F, B, Err> H1Service<Hst, Layer<TlsServerFilter, F>, B, Err>
    where
        F: Filter,
        B: MessageBody,
        Err: ResponseError + 'static,
    {
        /// Create rustls based service
        pub fn rustls(
            self,
            config: ServerConfig,
        ) -> impl Service<Hst, Req = Io<F>, Res = (), Error = SslError<DispatchError>> {
            TlsAcceptor::new(std::sync::Arc::new(config))
                .map_err(|e| SslError::Ssl(Box::new(e)))
                .and_then(self.map_err(SslError::Service))
        }
    }
}

impl<St, F, B, Err> H1Service<St, F, B, Err>
where
    F: Filter,
    B: MessageBody,
    Err: 'static,
{
    /// Provide http/1 control service.
    pub fn control<Ctl>(self, ctl: impl IntoService<Ctl, St>) -> H1Service<St, F, B, Err>
    where
        St: State<Control<F, Err>>,
        Ctl: Service<St, Req = Control<F, Err>, Res = ControlAck<F>> + 'static,
        Ctl::Error: Error + 'static,
    {
        H1Service {
            sf: self.sf,
            config: self.config,
            ctl: Pipeline::with(ctl.into_service().map_err(dyn_rc_err)),
            _t: marker::PhantomData,
        }
    }
}

impl<Hst, F, B, Err> Service<Hst> for H1Service<Hst, F, B, Err>
where
    F: Filter,
    B: MessageBody,
    Err: ResponseError + 'static,
{
    type Req = Io<F>;
    type Res = ();
    type Error = DispatchError;

    async fn ready(&self, _: ReadyCtx<'_, Self, Hst>) -> Result<(), Self::Error> {
        self.ctl.ready().await.map_err(|e| {
            log::error!("Http control service readiness error: {e:?}");
            DispatchError::Control(e)
        })
    }

    async fn shutdown(&self) {
        self.config.shutdown();

        // check inflight connections
        let inflight = self.config.shutdown();
        println!("=============== SHT ======== {inflight:?}");

        if inflight != 0 {
            log::trace!("Shutting down service, in-flight connections: {inflight}");

            self.config.wait_shutdown().await;
            log::trace!("Shutting down is complected");
        }

        self.ctl.shutdown().await;
    }

    async fn call(&self, io: Io<F>, ctx: Ctx<'_, Self, Hst>) -> Result<(), Self::Error> {
        let cfg = io.shared();
        let svc = self.sf.create(&cfg, ctx.st()).await.map_err(|e| {
            log::error!("Cannot construct handler service: {e:?}");
            DispatchError::Control(e)
        })?;

        let id = self.config.next_id();
        let ioref = io.get_ref();
        let inflight = self.config.insert_io(&ioref);
        println!("=============== START ======== {inflight:?}");

        log::trace!(
            "{}: New http1 connection {id}, peer address {:?}, inflight: {}",
            io.tag(),
            io.query::<types::PeerAddr>().get(),
            inflight
        );

        let result = handle_io(id, io, svc, self.ctl.bind(), self.config.clone()).await;

        let inflight = self.config.remove_io(&ioref);
        println!(
            "=============== STOP ======== {inflight:?} = {:?}",
            self.config.is_shutdown()
        );

        if inflight == 0 && self.config.is_shutdown() {
            self.config.notify_shutdown()
        }
        result
    }
}

pub(crate) async fn handle_io<F, B, Err>(
    id: usize,
    io: Io<F>,
    svc: Pipeline<Request, Response<B>, Err>,
    ctl: PipelineBinding<Control<F, Err>, ControlAck<F>, Rc<dyn Error>>,
    config: DispatcherConfig,
) -> Result<(), DispatchError>
where
    F: Filter,
    B: MessageBody,
    Err: ResponseError + 'static,
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
        Err(e) => Err(DispatchError::Control(e)),
    }
}
