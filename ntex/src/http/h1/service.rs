use crate::error::{Error, ErrorDiagnostic, ErrorInfo};
use crate::http::error::{DispatchError, ResponseError};
use crate::http::{Request, Response, config::DispatcherConfig};
use crate::io::{Filter, Io, types};
use crate::service::pipeline::{Pipeline, PipelineFactory};
use crate::service::{Ctx, IntoServiceFactory, RequestState, Service, ServiceFactory};

use super::control::{Control, ControlAck, ControlResult};
use super::default::DefaultControlService;
use super::dispatcher::Dispatcher;

/// `ServiceFactory` implementation for HTTP1 transport
#[derive(derive_more::Debug)]
#[debug("H1Service")]
pub struct H1Service<F, Req: RequestState<Io<F>>, Err> {
    sf: crate::http::HttpPipeline<Req::State, Err>,
    ctl: crate::http::Ctl1Pipeline<Req::State, F, Err>,
    config: DispatcherConfig,
}

impl<F, Req, Err> H1Service<F, Req, Err>
where
    F: Filter,
    Req: RequestState<Io<F>>,
    Req::State: Clone,
    Err: ResponseError + 'static,
{
    /// Create new `HttpService` instance with config.
    pub(crate) fn new<Sf>(sf: impl IntoServiceFactory<Sf, Req::State, Request>) -> Self
    where
        Sf: ServiceFactory<Req::State, Request, Error = Err> + 'static,
        Sf::Res: Into<Response>,
        Sf::InitError: ErrorDiagnostic,
    {
        H1Service {
            sf: PipelineFactory::new(
                sf.into_factory()
                    .map(Into::into)
                    .map_init_err(|e| DispatchError::Control(ErrorInfo::from(Error::from(e)))),
            ),
            ctl: PipelineFactory::new(DefaultControlService),
            config: DispatcherConfig::default(),
        }
    }
}

impl<F, Req, Err> H1Service<F, Req, Err>
where
    F: Filter,
    Req: RequestState<Io<F>>,
    Req::State: Clone,
    Err: ResponseError + 'static,
{
    #[must_use]
    /// Provide http/1 control service.
    pub fn control<I, Sf>(self, ctl: I) -> Self
    where
        I: IntoServiceFactory<Sf, Req::State, Control<F, Err>>,
        Sf: ServiceFactory<Req::State, Control<F, Err>, Res = ControlAck<F>> + 'static,
        Sf::Error: ErrorDiagnostic,
        Sf::InitError: ErrorDiagnostic,
    {
        H1Service {
            sf: self.sf,
            ctl: PipelineFactory::new(
                ctl.into_factory()
                    .map_err(|e| DispatchError::Service(ErrorInfo::from(Error::from(e))))
                    .map_init_err(|e| DispatchError::Control(ErrorInfo::from(Error::from(e)))),
            ),
            config: self.config,
        }
    }
}

impl<St, F, Req, Err> Service<St, Req> for H1Service<F, Req, Err>
where
    F: Filter,
    Req: RequestState<Io<F>>,
    Req::State: Clone,
    Err: ResponseError + 'static,
{
    type Res = ();
    type Error = DispatchError;

    async fn call(&self, req: Req, _: Ctx<'_, Self, St>) -> Result<(), Self::Error> {
        let (st, io) = req.unpack();

        let svc = self.sf.create(st.clone()).await?;
        let ctl = self.ctl.create(st).await?;

        let id = self.config.next_id();
        let ioref = io.get_ref();
        let inflight = self.config.insert_io(&ioref);

        log::trace!(
            "{}: New http1 connection {id}, peer address {:?}, inflight: {}",
            io.tag(),
            io.query::<types::PeerAddr>().get(),
            inflight
        );

        let result = handle_io(id, io, svc, ctl, self.config.clone()).await;

        let inflight = self.config.remove_io(&ioref);
        if inflight == 0 && self.config.is_shutdown() {
            self.config.notify_shutdown();
        }
        result
    }

    async fn ready(&self, _: Ctx<'_, Self, St>) -> Result<(), Self::Error> {
        Ok(())
    }

    async fn shutdown(&self, _: crate::Ctx<'_, Self, St>) {
        self.config.shutdown();

        // check inflight connections
        let inflight = self.config.shutdown();
        if inflight != 0 {
            log::trace!("Shutting down service, in-flight connections: {inflight}");

            self.config.wait_shutdown().await;
            log::trace!("Shutting down is complected");
        }
    }
}

pub(crate) async fn handle_io<F, Err>(
    id: usize,
    io: Io<F>,
    svc: Pipeline<Request, Response, Err>,
    ctl: Pipeline<Control<F, Err>, ControlAck<F>, DispatchError>,
    config: DispatcherConfig,
) -> Result<(), DispatchError>
where
    F: Filter,
    Err: ResponseError + 'static,
{
    // Notify control service
    let ack = ctl.call_nowait(Control::connect(id, io)).await?;
    let ControlResult::Connect(io) = ack.result else {
        unreachable!();
    };

    Dispatcher::new(id, io, svc, ctl, config).await
}
