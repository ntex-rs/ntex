use std::{error::Error, marker::PhantomData};

use crate::io::{Filter, Io, types};
use crate::service::{IntoServiceFactory, Pipeline, RequestState};
use crate::{Ctx, Service, ServiceFactory, util::dyn_rc_err};

use super::error::{DispatchError, H2Error, ResponseError};
use super::{Request, Response, config::DispatcherConfig, h1, h2};

/// HTTP1.1/HTTP2 transport implementation
#[derive(derive_more::Debug)]
#[debug("HttpService")]
pub struct HttpService<St, F, Req: RequestState<Io<F>>, H, Ctl1, Ctl2> {
    sf: H,
    h1_ctl: Ctl1,
    h2_ctl: Ctl2,
    config: DispatcherConfig,
    ph: PhantomData<(St, F, Req)>,
}

impl<St, F, Req, H> HttpService<St, F, Req, H, h1::DefaultControlService, h2::DefaultControlService>
where
    St: 'static,
    F: Filter + 'static,
    Req: RequestState<Io<F>>,
    Req::State: Clone,
    H: 'static,
{
    #[must_use]
    /// Create new `HttpService` instance.
    pub fn new(sf: impl IntoServiceFactory<H, Req::State, Request, St>) -> Self
    where
        H: ServiceFactory<Req::State, Request, St>,
        H::Res: Into<Response>,
        H::Error: ResponseError,
        H::InitError: Error,
    {
        HttpService {
            sf: sf.into_factory(),
            h1_ctl: h1::DefaultControlService,
            h2_ctl: h2::DefaultControlService,
            config: DispatcherConfig::default(),
            ph: PhantomData,
        }
    }

    #[must_use]
    /// Create *http service* for HTTP/1 protocol.
    pub fn h1(
        sf: impl IntoServiceFactory<H, Req::State, Request, St>,
    ) -> h1::H1Service<St, F, Req, H>
    where
        H: ServiceFactory<Req::State, Request, St> + 'static,
        H::Res: Into<Response>,
        H::Error: ResponseError,
        H::InitError: Error,
    {
        h1::H1Service::new(sf)
    }

    #[must_use]
    /// Create *http service* for HTTP/2 protocol.
    pub fn h2(
        sf: impl IntoServiceFactory<H, Req::State, Request, St>,
    ) -> h2::H2Service<St, F, Req, H>
    where
        St: 'static,
        H: ServiceFactory<Req::State, Request, St> + 'static,
        H::Res: Into<Response>,
        H::Error: ResponseError,
        H::InitError: Error,
    {
        h2::H2Service::new(sf)
    }
}

impl<St, F, Req, H, Ctl1, Ctl2> HttpService<St, F, Req, H, Ctl1, Ctl2>
where
    St: 'static,
    F: Filter,
    Req: RequestState<Io<F>>,
    H: 'static,
    Ctl1: 'static,
    Ctl2: 'static,
{
    #[must_use]
    /// Provide http/1 control service.
    pub fn h1_control<Ctl, I>(self, ctl: I) -> HttpService<St, F, Req, H, Ctl, Ctl2>
    where
        H: ServiceFactory<Req::State, Request, St>,
        Ctl: ServiceFactory<Req::State, h1::Control<F, H::Error>, St, Res = h1::ControlAck<F>>,
        Ctl::Error: Error,
        Ctl::InitError: Error,
        I: IntoServiceFactory<Ctl, Req::State, h1::Control<F, H::Error>, St>,
    {
        HttpService {
            sf: self.sf,
            h1_ctl: ctl.into_factory(),
            h2_ctl: self.h2_ctl,
            config: self.config,
            ph: self.ph,
        }
    }

    #[must_use]
    /// Provide http/1 control service.
    pub fn h2_control<Ctl, I>(self, ctl: I) -> HttpService<St, F, Req, H, Ctl1, Ctl>
    where
        Ctl: ServiceFactory<Req::State, h2::Control<H2Error>, St, Res = h2::ControlAck> + 'static,
        Ctl::Error: Error,
        Ctl::InitError: Error,
        I: IntoServiceFactory<Ctl, Req::State, h2::Control<H2Error>, St>,
    {
        HttpService {
            sf: self.sf,
            h1_ctl: self.h1_ctl,
            h2_ctl: ctl.into_factory(),
            config: self.config,
            ph: self.ph,
        }
    }
}

impl<St, F, Req, H, Ctl1, Ctl2> HttpService<St, F, Req, H, Ctl1, Ctl2>
where
    St: 'static,
    F: Filter + 'static,
    Req: RequestState<Io<F>>,
    Req::State: Clone,
    H: ServiceFactory<Req::State, Request, St> + 'static,
    H::Res: Into<Response>,
    H::Error: ResponseError,
    H::InitError: Error,
    Ctl1:
        ServiceFactory<Req::State, h1::Control<F, H::Error>, St, Res = h1::ControlAck<F>> + 'static,
    Ctl1::Error: Error,
    Ctl1::InitError: Error,
    Ctl2: ServiceFactory<Req::State, h2::Control<H2Error>, St, Res = h2::ControlAck> + 'static,
    Ctl2::Error: Error,
    Ctl2::InitError: Error,
{
    pub fn build(self) -> impl Service<St, Req, Res = (), Error = DispatchError> {
        self
    }
}

impl<St, F, Req, H, Ctl1, Ctl2> Service<St, Req> for HttpService<St, F, Req, H, Ctl1, Ctl2>
where
    St: 'static,
    F: Filter + 'static,
    Req: RequestState<Io<F>>,
    Req::State: Clone,
    H: ServiceFactory<Req::State, Request, St> + 'static,
    H::Res: Into<Response>,
    H::Error: ResponseError,
    H::InitError: Error,
    Ctl1:
        ServiceFactory<Req::State, h1::Control<F, H::Error>, St, Res = h1::ControlAck<F>> + 'static,
    Ctl1::Error: Error,
    Ctl1::InitError: Error,
    Ctl2: ServiceFactory<Req::State, h2::Control<H2Error>, St, Res = h2::ControlAck> + 'static,
    Ctl2::Error: Error,
    Ctl2::InitError: Error,
{
    type Res = ();
    type Error = DispatchError;

    async fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> Result<Self::Res, Self::Error> {
        let (io, st) = req.unpack();

        let svc = self.sf.create(ctx.st()).await.map_err(|e| {
            log::error!("Cannot construct handler service: {e:?}");
            DispatchError::Control
        })?;

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

            let ctl = self.h2_ctl.create(ctx.st()).await.map_err(|e| {
                log::error!("Cannot construct h2 control service: {e:?}");
                DispatchError::Control
            })?;

            h2::handle(
                id,
                io.into(),
                Pipeline::with(st.clone(), svc.map(Into::into)),
                Pipeline::with(st, ctl.map_err(dyn_rc_err)),
            )
            .await
        } else {
            log::trace!(
                "{}: New http1 connection {id}, peer address {:?}, in-flight: {inflight}",
                io.tag(),
                io.query::<types::PeerAddr>().get(),
            );

            let ctl = self.h1_ctl.create(ctx.st()).await.map_err(|e| {
                log::error!("Cannot construct h1 control service: {e:?}");
                DispatchError::Control
            })?;

            h1::handle_io(
                id,
                io,
                Pipeline::with(st.clone(), svc.map(Into::into)),
                Pipeline::with(st, ctl.map_err(dyn_rc_err)),
                self.config.clone(),
            )
            .await
        };

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
        // check inflight connections
        let inflight = self.config.shutdown();
        if inflight != 0 {
            log::trace!("Shutting down service, in-flight connections: {inflight}");

            self.config.wait_shutdown().await;
            log::trace!("Shutting down is complected");
        }
    }
}
