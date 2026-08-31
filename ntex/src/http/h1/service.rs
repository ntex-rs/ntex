use std::{error::Error, rc::Rc};

use crate::http::config::DispatcherConfig;
use crate::http::error::{DispatchError, ResponseError};
use crate::http::{HttpPipeline, body::MessageBody, request::Request, response::Response};
use crate::io::{Filter, Io, types};
use crate::service::pipeline::{Pipeline, PipelineBinding, PipelineState};
use crate::service::state::RequestState;
use crate::service::{
    Ctx, IntoService, IntoServiceFactory, Service, ServiceFactory, cfg::SharedCfg,
};
use crate::util::dyn_rc_err;

use super::control::{Control, ControlAck, ControlResult};
use super::default::DefaultControlService;
use super::dispatcher::Dispatcher;

/// `ServiceFactory` implementation for HTTP1 transport
#[derive(derive_more::Debug)]
#[debug("H1Service")]
pub struct H1Service<St, Rst: RequestState<Res = Io<F>>, F, B, Err> {
    rst: Rst,
    sf: HttpPipeline<Rst::State, B, Err>,
    ctl: PipelineState<St, Control<F, Err>, ControlAck<F>, Rc<dyn Error>>,
    config: DispatcherConfig,
}

impl<St, Rst, F, B, Err> H1Service<St, Rst, F, B, Err>
where
    St: Clone + 'static,
    Rst: RequestState<Res = Io<F>>,
    F: Filter,
    B: MessageBody,
    Err: ResponseError + 'static,
{
    /// Create new `HttpService` instance with config.
    pub(crate) fn new<Sf>(
        rst: Rst,
        sf: impl IntoServiceFactory<Sf, Rst::State, Request, SharedCfg>,
    ) -> H1Service<St, Rst, F, B, Err>
    where
        Sf: ServiceFactory<Rst::State, Request, SharedCfg, Error = Err> + 'static,
        Sf::Res: Into<Response<B>>,
        Sf::InitError: Into<Box<dyn Error>>,
    {
        H1Service {
            rst,
            sf: HttpPipeline::new(sf.into_factory().map(Into::into).map_init_err(Into::into)),
            ctl: PipelineState::new(DefaultControlService),
            config: DispatcherConfig::default(),
        }
    }
}

impl<St, Rst, F, B, Err> H1Service<St, Rst, F, B, Err>
where
    St: Clone + 'static,
    Rst: RequestState<Res = Io<F>>,
    F: Filter,
    B: MessageBody,
    Err: 'static,
{
    #[must_use]
    /// Provide http/1 control service.
    pub fn control<Ctl>(self, ctl: impl IntoService<Ctl, St, Control<F, Err>>) -> Self
    where
        Ctl: Service<St, Control<F, Err>, Res = ControlAck<F>> + 'static,
        Ctl::Error: Error + 'static,
    {
        H1Service {
            sf: self.sf,
            rst: self.rst,
            config: self.config,
            ctl: PipelineState::new(ctl.into_service().map_err(dyn_rc_err)),
        }
    }
}

impl<St, Rst, F, B, Err> Service<St, Rst::Req> for H1Service<St, Rst, F, B, Err>
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
        self.ctl.ready(ctx.st()).await.map_err(|e| {
            log::error!("Http control service readiness error: {e:?}");
            DispatchError::Control
        })
    }

    async fn shutdown(&self, ctx: crate::Ctx<'_, Self, St>) {
        self.config.shutdown();

        // check inflight connections
        let inflight = self.config.shutdown();
        if inflight != 0 {
            log::trace!("Shutting down service, in-flight connections: {inflight}");

            self.config.wait_shutdown().await;
            log::trace!("Shutting down is complected");
        }

        self.ctl.shutdown(ctx.st()).await;
    }

    async fn call(&self, io: Rst::Req, ctx: Ctx<'_, Self, St>) -> Result<(), Self::Error> {
        let (io, st) = self.rst.map(io).await.map_err(|_| {
            log::error!("Cannot extract state");
            DispatchError::Control
        })?;

        let cfg = io.shared();
        let svc = self.sf.create(&cfg, st).await.map_err(|e| {
            log::error!("Cannot construct handler service: {e:?}");
            DispatchError::Control
        })?;

        let id = self.config.next_id();
        let ioref = io.get_ref();
        let inflight = self.config.insert_io(&ioref);

        log::trace!(
            "{}: New http1 connection {id}, peer address {:?}, inflight: {}",
            io.tag(),
            io.query::<types::PeerAddr>().get(),
            inflight
        );

        let result = handle_io(
            id,
            io,
            svc,
            self.ctl.bind_state(ctx.st().clone()),
            self.config.clone(),
        )
        .await;

        let inflight = self.config.remove_io(&ioref);
        if inflight == 0 && self.config.is_shutdown() {
            self.config.notify_shutdown();
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
                .map_err(|_| DispatchError::Control)
        }
        Err(e) => {
            log::error!("Control service error: {e:?}");
            Err(DispatchError::Control)
        }
    }
}
