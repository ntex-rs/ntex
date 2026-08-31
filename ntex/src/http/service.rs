use std::{error::Error, rc::Rc};

use crate::io::{Filter, Io, types};
use crate::service::pipeline::PipelineState;
use crate::service::state::{DefaultState, RequestState};
use crate::service::{IntoService, IntoServiceFactory};
use crate::{Ctx, Service, ServiceFactory, SharedCfg, util::join};

use super::error::{DispatchError, H2Error, ResponseError};
use super::{HttpPipeline, h1, h2, request::Request, response::Response};
use super::{body::MessageBody, config::DispatcherConfig};

/// HTTP1.1/HTTP2 transport implementation
#[derive(derive_more::Debug)]
#[debug("HttpService")]
pub struct HttpService<St, Rst: RequestState, F, B, Err> {
    rst: Rst,
    sf: HttpPipeline<Rst::State, B, Err>,
    h1_ctl: PipelineState<St, h1::Control<F, Err>, h1::ControlAck<F>, Rc<dyn Error>>,
    h2_ctl: PipelineState<St, h2::Control<H2Error>, h2::ControlAck, Rc<dyn Error>>,
    config: DispatcherConfig,
}

impl<St, F, B, Err> HttpService<St, DefaultState<Io<F>, ()>, F, B, Err>
where
    St: Clone + 'static,
    F: Filter,
    B: MessageBody,
    Err: ResponseError + 'static,
{
    #[must_use]
    /// Create new `HttpService` instance.
    pub fn new<Sf>(sf: impl IntoServiceFactory<Sf, (), Request, SharedCfg>) -> Self
    where
        Sf: ServiceFactory<(), Request, SharedCfg, Error = Err> + 'static,
        Sf::Res: Into<Response<B>>,
        Sf::InitError: Into<Box<dyn Error>>,
    {
        HttpService {
            rst: DefaultState::new(),
            sf: HttpPipeline::new(sf.into_factory().map(Into::into).map_init_err(Into::into)),
            h1_ctl: PipelineState::new(h1::DefaultControlService),
            h2_ctl: PipelineState::new(h2::DefaultControlService),
            config: DispatcherConfig::default(),
        }
    }

    #[must_use]
    /// Create *http service* for HTTP/1 protocol.
    pub fn h1<Sf>(
        sf: impl IntoServiceFactory<Sf, (), Request, SharedCfg>,
    ) -> h1::H1Service<St, DefaultState<Io<F>, ()>, F, B, Err>
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
    ) -> h2::H2Service<St, DefaultState<Io<F>, ()>, F, B, Err>
    where
        Sf: ServiceFactory<(), Request, SharedCfg, Error = Err> + 'static,
        Sf::Res: Into<Response<B>>,
        Sf::InitError: Into<Box<dyn Error>>,
    {
        h2::H2Service::new(DefaultState::new(), sf)
    }
}

impl<St, Rst, F, B, Err> HttpService<St, Rst, F, B, Err>
where
    St: Clone + 'static,
    Rst: RequestState<Res = Io<F>>,
    F: Filter,
    B: MessageBody,
    Err: ResponseError + 'static,
{
    #[must_use]
    /// Create new `HttpService` instance.
    pub fn with<Sf>(
        rst: Rst,
        sf: impl IntoServiceFactory<Sf, Rst::State, Request, SharedCfg>,
    ) -> HttpService<St, Rst, F, B, Err>
    where
        Sf: ServiceFactory<Rst::State, Request, SharedCfg, Error = Err> + 'static,
        Sf::Res: Into<Response<B>>,
        Sf::InitError: Into<Box<dyn Error>>,
    {
        HttpService {
            rst,
            sf: HttpPipeline::new(sf.into_factory().map(Into::into).map_init_err(Into::into)),
            h1_ctl: PipelineState::new(h1::DefaultControlService),
            h2_ctl: PipelineState::new(h2::DefaultControlService),
            config: DispatcherConfig::default(),
        }
    }

    #[must_use]
    /// Create *http service* for HTTP/1 protocol.
    pub fn h1_with<Sf>(
        rst: Rst,
        sf: impl IntoServiceFactory<Sf, Rst::State, Request, SharedCfg>,
    ) -> h1::H1Service<St, Rst, F, B, Err>
    where
        Sf: ServiceFactory<Rst::State, Request, SharedCfg, Error = Err> + 'static,
        Sf::Res: Into<Response<B>>,
        Sf::InitError: Into<Box<dyn Error>>,
    {
        h1::H1Service::new(rst, sf)
    }

    #[must_use]
    /// Create *http service* for HTTP/2 protocol.
    pub fn h2_with<Sf>(
        rst: Rst,
        sf: impl IntoServiceFactory<Sf, Rst::State, Request, SharedCfg>,
    ) -> h2::H2Service<St, Rst, F, B, Err>
    where
        Sf: ServiceFactory<Rst::State, Request, SharedCfg, Error = Err> + 'static,
        Sf::Res: Into<Response<B>>,
        Sf::InitError: Into<Box<dyn Error>>,
    {
        h2::H2Service::new(rst, sf)
    }
}

impl<St, Rst, F, B, Err> HttpService<St, Rst, F, B, Err>
where
    St: 'static,
    Rst: RequestState<Res = Io<F>>,
    F: Filter,
    B: MessageBody,
    Err: ResponseError + 'static,
{
    #[must_use]
    /// Provide http/1 control service.
    pub fn h1_control<Ctl>(
        self,
        ctl: impl IntoService<Ctl, St, h1::Control<F, Err>>,
    ) -> HttpService<St, Rst, F, B, Err>
    where
        Ctl: Service<St, h1::Control<F, Err>, Res = h1::ControlAck<F>> + 'static,
        Ctl::Error: Into<Rc<dyn Error>> + 'static,
    {
        HttpService {
            sf: self.sf,
            rst: self.rst,
            config: self.config,
            h1_ctl: PipelineState::new(ctl.into_service().map_err(Into::into)),
            h2_ctl: self.h2_ctl,
        }
    }

    #[must_use]
    /// Provide http/1 control service.
    pub fn h2_control<Ctl>(
        self,
        ctl: impl IntoService<Ctl, St, h2::Control<H2Error>>,
    ) -> HttpService<St, Rst, F, B, Err>
    where
        Ctl: Service<St, h2::Control<H2Error>, Res = h2::ControlAck> + 'static,
        Ctl::Error: Into<Rc<dyn Error>> + 'static,
    {
        HttpService {
            sf: self.sf,
            rst: self.rst,
            config: self.config,
            h1_ctl: self.h1_ctl,
            h2_ctl: PipelineState::new(ctl.into_service().map_err(Into::into)),
        }
    }
}

impl<St, Rst, F, B, Err> Service<St, Rst::Req> for HttpService<St, Rst, F, B, Err>
where
    St: Clone + 'static,
    Rst: RequestState<Res = Io<F>>,
    F: Filter,
    B: MessageBody,
    Err: ResponseError + 'static,
{
    type Res = ();
    type Error = DispatchError;

    async fn ready(&self, ctx: Ctx<'_, Self, St>) -> Result<(), Self::Error> {
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

    async fn shutdown(&self, ctx: crate::Ctx<'_, Self, St>) {
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

    async fn call(&self, io: Rst::Req, ctx: Ctx<'_, Self, St>) -> Result<Self::Res, Self::Error> {
        let (io, st) = self.rst.map(io).await.map_err(|_| {
            log::error!("Cannot derive state");
            DispatchError::Control
        })?;

        let cfg = io.shared();
        let svc = self.sf.create(&cfg, st).await.map_err(|e| {
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
