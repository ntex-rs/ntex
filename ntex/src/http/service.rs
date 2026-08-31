use std::{error::Error, rc::Rc};

use crate::io::{Filter, Io, types};
use crate::service::pipeline::PipelineState;
use crate::service::state::{DefaultState, State, StateMapping};
use crate::service::{IntoService, IntoServiceFactory};
use crate::{Ctx, Service, ServiceFactory, SharedCfg, util::join};

use super::error::{DispatchError, H2Error, ResponseError};
use super::{HttpPipeline, h1, h2, request::Request, response::Response};
use super::{body::MessageBody, config::DispatcherConfig};

/// HTTP1.1/HTTP2 transport implementation
#[derive(derive_more::Debug)]
#[debug("HttpService")]
pub struct HttpService<Hst, F, B, Err> {
    sf: HttpPipeline<Hst, B, Err>,
    h1_ctl: PipelineState<Hst, h1::Control<F, Err>, h1::ControlAck<F>, Rc<dyn Error>>,
    h2_ctl: PipelineState<Hst, h2::Control<H2Error>, h2::ControlAck, Rc<dyn Error>>,
    config: DispatcherConfig,
}

impl<Hst, F, B, Err> HttpService<Hst, F, B, Err>
where
    Hst: 'static,
    F: Filter,
    B: MessageBody,
    Err: ResponseError + 'static,
{
    #[must_use]
    /// Create new `HttpService` instance.
    pub fn new<Sf>(
        sf: impl IntoServiceFactory<Sf, (), Request, SharedCfg>,
    ) -> HttpService<Hst, F, B, Err>
    where
        Sf: ServiceFactory<(), Request, SharedCfg, Error = Err> + 'static,
        Sf::Res: Into<Response<B>>,
        Sf::InitError: Into<Box<dyn Error>>,
    {
        HttpService {
            sf: HttpPipeline::with(
                DefaultState::new(),
                sf.into_factory().map(Into::into).map_init_err(Into::into),
            ),
            h1_ctl: PipelineState::new(h1::DefaultControlService),
            h2_ctl: PipelineState::new(h2::DefaultControlService),
            config: DispatcherConfig::default(),
        }
    }

    #[must_use]
    /// Create new `HttpService` instance.
    pub fn with<Sf, Sm>(
        sm: Sm,
        sf: impl IntoServiceFactory<Sf, Sm::State, Request, SharedCfg>,
    ) -> HttpService<Hst, F, B, Err>
    where
        Sm: StateMapping<Hst>,
        Sm::Control: State<Sm::State, Request>,
        Sf: ServiceFactory<Sm::State, Request, SharedCfg, Error = Err> + 'static,
        Sf::Res: Into<Response<B>>,
        Sf::InitError: Into<Box<dyn Error>>,
    {
        HttpService {
            sf: HttpPipeline::with(
                sm,
                sf.into_factory().map(Into::into).map_init_err(Into::into),
            ),
            h1_ctl: PipelineState::new(h1::DefaultControlService),
            h2_ctl: PipelineState::new(h2::DefaultControlService),
            config: DispatcherConfig::default(),
        }
    }
}

impl<Hst, F, B, Err> HttpService<Hst, F, B, Err>
where
    Hst: 'static,
    F: Filter,
    B: MessageBody,
    Err: ResponseError + 'static,
{
    #[must_use]
    /// Create *http service* for HTTP/1 protocol.
    pub fn h1<Sf>(
        sf: impl IntoServiceFactory<Sf, (), Request, SharedCfg>,
    ) -> h1::H1Service<Hst, F, B, Err>
    where
        Sf: ServiceFactory<(), Request, SharedCfg, Error = Err> + 'static,
        Sf::Res: Into<Response<B>>,
        Sf::InitError: Into<Box<dyn Error>>,
    {
        h1::H1Service::new(DefaultState::new(), sf)
    }

    #[must_use]
    /// Create *http service* for HTTP/2 protocol.
    pub fn h2<Sf>(
        sf: impl IntoServiceFactory<Sf, (), Request, SharedCfg>,
    ) -> h2::H2Service<Hst, F, B, Err>
    where
        Sf: ServiceFactory<(), Request, SharedCfg, Error = Err> + 'static,
        Sf::Res: Into<Response<B>>,
        Sf::InitError: Into<Box<dyn Error>>,
    {
        h2::H2Service::new(DefaultState::new(), sf)
    }

    #[must_use]
    /// Create *http service* for HTTP/1 protocol.
    pub fn h1_with<Sf, Sm>(
        sm: Sm,
        sf: impl IntoServiceFactory<Sf, Sm::State, Request, SharedCfg>,
    ) -> h1::H1Service<Hst, F, B, Err>
    where
        Sm: StateMapping<Hst>,
        Sm::Control: State<Sm::State, Request>,
        Sf: ServiceFactory<Sm::State, Request, SharedCfg, Error = Err> + 'static,
        Sf::Res: Into<Response<B>>,
        Sf::InitError: Into<Box<dyn Error>>,
    {
        h1::H1Service::new(sm, sf)
    }

    #[must_use]
    /// Create *http service* for HTTP/2 protocol.
    pub fn h2_with<Sf, Sm>(
        sm: Sm,
        sf: impl IntoServiceFactory<Sf, Sm::State, Request, SharedCfg>,
    ) -> h2::H2Service<Hst, F, B, Err>
    where
        Sf: ServiceFactory<Sm::State, Request, SharedCfg, Error = Err> + 'static,
        Sf::Res: Into<Response<B>>,
        Sf::InitError: Into<Box<dyn Error>>,
        Sm: StateMapping<Hst>,
        Sm::Control: State<Sm::State, Request>,
    {
        h2::H2Service::new(sm, sf)
    }
}

impl<Hst, F, B, Err> HttpService<Hst, F, B, Err>
where
    Hst: 'static,
    F: Filter,
    B: MessageBody,
    Err: ResponseError + 'static,
{
    #[must_use]
    /// Provide http/1 control service.
    pub fn h1_control<Ctl>(
        self,
        ctl: impl IntoService<Ctl, Hst, h1::Control<F, Err>>,
    ) -> HttpService<Hst, F, B, Err>
    where
        Ctl: Service<Hst, h1::Control<F, Err>, Res = h1::ControlAck<F>> + 'static,
        Ctl::Error: Into<Rc<dyn Error>> + 'static,
    {
        HttpService {
            sf: self.sf,
            config: self.config,
            h1_ctl: PipelineState::new(ctl.into_service().map_err(Into::into)),
            h2_ctl: self.h2_ctl,
        }
    }

    #[must_use]
    /// Provide http/1 control service.
    pub fn h2_control<Ctl>(
        self,
        ctl: impl IntoService<Ctl, Hst, h2::Control<H2Error>>,
    ) -> HttpService<Hst, F, B, Err>
    where
        Ctl: Service<Hst, h2::Control<H2Error>, Res = h2::ControlAck> + 'static,
        Ctl::Error: Into<Rc<dyn Error>> + 'static,
    {
        HttpService {
            sf: self.sf,
            config: self.config,
            h1_ctl: self.h1_ctl,
            h2_ctl: PipelineState::new(ctl.into_service().map_err(Into::into)),
        }
    }
}

impl<Hst, F, B, Err> Service<Hst, Io<F>> for HttpService<Hst, F, B, Err>
where
    Hst: Clone + 'static,
    F: Filter,
    B: MessageBody,
    Err: ResponseError + 'static,
{
    type Res = ();
    type Error = DispatchError;

    async fn ready(&self, ctx: Ctx<'_, Self, Hst>) -> Result<(), Self::Error> {
        let (r1, r2) = join(self.h1_ctl.ready(ctx.st()), self.h2_ctl.ready(ctx.st())).await;
        r1.map_err(|e| {
            log::error!("Http control service readiness error: {e:?}");
            DispatchError::Control
        })?;
        r2.map_err(|e| {
            log::error!("Http control service readiness error: {e:?}");
            DispatchError::Control
        })?;
        Ok(())
    }

    async fn shutdown(&self, ctx: crate::Ctx<'_, Self, Hst>) {
        // check inflight connections
        let inflight = self.config.shutdown();
        if inflight != 0 {
            log::trace!("Shutting down service, in-flight connections: {inflight}");

            self.config.wait_shutdown().await;
            log::trace!("Shutting down is complected");
        }

        // shutdown control services
        join(
            self.h1_ctl.shutdown(ctx.st()),
            self.h2_ctl.shutdown(ctx.st()),
        )
        .await;
    }

    async fn call(&self, io: Io<F>, ctx: Ctx<'_, Self, Hst>) -> Result<Self::Res, Self::Error> {
        let cfg = io.shared();
        let svc = self.sf.create(&cfg, ctx.st()).await.map_err(|e| {
            log::error!("Cannot construct handler service: {e:?}");
            DispatchError::Control
        })?;

        let st = ctx.st().clone();
        let id = self.config.next_id();
        let ioref = io.get_ref();
        let inflight = self.config.insert_io(&ioref);

        let result = if io.query::<types::HttpProtocol>().get() == Some(types::HttpProtocol::Http2)
        {
            log::trace!(
                "{}: New http2 connection {id}, peer address {:?}, in-flight: {inflight}",
                io.tag(),
                io.query::<types::PeerAddr>().get(),
            );

            h2::handle(id, io.into(), svc, self.h2_ctl.bind_state(st)).await
        } else {
            log::trace!(
                "{}: New http1 connection {id}, peer address {:?}, in-flight: {inflight}",
                io.tag(),
                io.query::<types::PeerAddr>().get(),
            );

            h1::handle_io(id, io, svc, self.h1_ctl.bind_state(st), self.config.clone()).await
        };

        let inflight = self.config.remove_io(&ioref);
        if inflight == 0 && self.config.is_shutdown() {
            self.config.notify_shutdown();
        }
        result
    }
}
