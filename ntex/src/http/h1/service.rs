use std::{error::Error, marker::PhantomData, rc::Rc};

use crate::http::error::{DispatchError, ResponseError};
use crate::http::{Request, Response, config::DispatcherConfig};
use crate::io::{Filter, Io, types};
use crate::service::{Ctx, IntoServiceFactory, Pipeline, RequestState, Service, ServiceFactory};
use crate::util::dyn_rc_err;

use super::control::{Control, ControlAck, ControlResult};
use super::default::DefaultControlService;
use super::dispatcher::Dispatcher;

/// `ServiceFactory` implementation for HTTP1 transport
#[derive(derive_more::Debug)]
#[debug("H1Service")]
pub struct H1Service<F, Req: RequestState<Io<F>>, H, Ctl = DefaultControlService> {
    sf: H,
    ctl: Ctl,
    config: DispatcherConfig,
    ph: PhantomData<(F, Req)>,
}

impl<F, Req, H> H1Service<F, Req, H>
where
    F: Filter,
    Req: RequestState<Io<F>>,
    Req::State: Clone,
{
    /// Create new `HttpService` instance with config.
    pub(crate) fn new(sf: impl IntoServiceFactory<H, Req::State, Request>) -> Self
    where
        H: ServiceFactory<Req::State, Request> + 'static,
        H::Res: Into<Response>,
        H::Error: ResponseError,
        H::InitError: Error,
    {
        H1Service {
            sf: sf.into_factory(),
            ctl: DefaultControlService,
            config: DispatcherConfig::default(),
            ph: PhantomData,
        }
    }
}

impl<F, Req, H, Ctl> H1Service<F, Req, H, Ctl>
where
    F: Filter,
    Req: RequestState<Io<F>>,
{
    #[must_use]
    /// Provide http/1 control service.
    pub fn control<I, Sf>(self, ctl: I) -> H1Service<F, Req, H, Sf>
    where
        H: ServiceFactory<Req::State, Request>,
        I: IntoServiceFactory<Sf, Req::State, Control<F, H::Error>>,
        Sf: ServiceFactory<Req::State, Control<F, H::Error>, Res = ControlAck<F>> + 'static,
        Sf::Error: Error,
        Sf::InitError: Error,
    {
        H1Service {
            sf: self.sf,
            ctl: ctl.into_factory(),
            config: self.config,
            ph: self.ph,
        }
    }
}

impl<St, F, Req, H, Ctl> Service<St, Req> for H1Service<F, Req, H, Ctl>
where
    F: Filter,
    Req: RequestState<Io<F>>,
    Req::State: Clone,
    H: ServiceFactory<Req::State, Request> + 'static,
    H::Res: Into<Response>,
    H::Error: ResponseError,
    H::InitError: Error,
    Ctl: ServiceFactory<Req::State, Control<F, H::Error>, Res = ControlAck<F>> + 'static,
    Ctl::Error: Error,
    Ctl::InitError: Error,
{
    type Res = ();
    type Error = DispatchError;

    async fn call(&self, req: Req, _: Ctx<'_, Self, St>) -> Result<(), Self::Error> {
        let (st, io) = req.unpack();

        let svc = self.sf.create(&st).await.map_err(|e| {
            log::error!("Cannot construct handler service: {e:?}");
            DispatchError::Control
        })?;
        let ctl = self.ctl.create(&st).await.map_err(|e| {
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
            Pipeline::new(st.clone(), svc.map(Into::into)),
            Pipeline::new(st, ctl.map_err(dyn_rc_err)),
            self.config.clone(),
        )
        .await;

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
    ctl: Pipeline<Control<F, Err>, ControlAck<F>, Rc<dyn Error>>,
    config: DispatcherConfig,
) -> Result<(), DispatchError>
where
    F: Filter,
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
