use std::{error::Error, marker, rc::Rc};

use crate::http::config::DispatcherConfig;
use crate::http::error::{DispatchError, ResponseError};
use crate::http::{HttpPipeline, body::MessageBody, request::Request, response::Response};
use crate::io::{Filter, Io, types};
use crate::service::pipeline::{Pipeline, PipelineBinding};
use crate::service::{
    Ctx, IntoService, IntoServiceFactory, Service, ServiceFactory, cfg::SharedCfg,
    state::DefaultState,
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
    Hst: 'static,
    F: Filter,
    B: MessageBody,
    Err: ResponseError + 'static,
{
    /// Create new `HttpService` instance with config.
    pub(crate) fn new<Sf>(
        sf: impl IntoServiceFactory<Sf, (), Request, SharedCfg>,
    ) -> H1Service<Hst, F, B, Err>
    where
        Sf: ServiceFactory<(), Request, SharedCfg, Error = Err> + 'static,
        Sf::Res: Into<Response<B>>,
        Sf::InitError: Error,
    {
        H1Service {
            sf: HttpPipeline::with(
                DefaultState,
                sf.into_factory().map(Into::into).map_init_err(dyn_rc_err),
            ),
            ctl: Pipeline::new(DefaultControlService),
            config: DispatcherConfig::default(),
            _t: marker::PhantomData,
        }
    }
}

impl<St, F, B, Err> H1Service<St, F, B, Err>
where
    F: Filter,
    B: MessageBody,
    Err: 'static,
{
    #[must_use]
    /// Provide http/1 control service.
    pub fn control<Ctl>(
        self,
        ctl: impl IntoService<Ctl, St, Control<F, Err>>,
    ) -> H1Service<St, F, B, Err>
    where
        St: Default + 'static,
        Ctl: Service<St, Control<F, Err>, Res = ControlAck<F>> + 'static,
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

impl<Hst, F, B, Err> Service<Hst, Io<F>> for H1Service<Hst, F, B, Err>
where
    F: Filter,
    B: MessageBody,
    Err: ResponseError + 'static,
{
    type Res = ();
    type Error = DispatchError;

    async fn ready(&self, _: Ctx<'_, Self, Hst>) -> Result<(), Self::Error> {
        self.ctl.ready().await.map_err(|e| {
            log::error!("Http control service readiness error: {e:?}");
            DispatchError::Control(e)
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

    async fn call(&self, io: Io<F>, ctx: Ctx<'_, Self, Hst>) -> Result<(), Self::Error> {
        let cfg = io.shared();
        let svc = self.sf.create(&cfg, ctx.st()).await.map_err(|e| {
            log::error!("Cannot construct handler service: {e:?}");
            DispatchError::Control(e)
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

        let result = handle_io(id, io, svc, self.ctl.bind(), self.config.clone()).await;

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
                .map_err(DispatchError::Control)
        }
        Err(e) => Err(DispatchError::Control(e)),
    }
}
