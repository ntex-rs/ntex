use std::{error::Error, marker, rc::Rc};

use crate::io::{Filter, Io, types};
use crate::service::{IntoService, IntoServiceFactory, Pipeline, PipelineFactory};
use crate::util::{dyn_rc_err, join};
use crate::{Ctx, ReadyCtx, Service, ServiceFactory, SharedCfg, State};

use super::error::{DispatchError, H2Error, ResponseError};
use super::{HttpPipeline, h1, h2, request::Request, response::Response};
use super::{body::MessageBody, config::DispatcherConfig};

/// HTTP1.1/HTTP2 transport implementation
#[derive(derive_more::Debug)]
#[debug("HttpService")]
pub struct HttpService<Hst, F, B, Err> {
    sf: HttpPipeline<Hst, B, Err>,
    h1_ctl: Pipeline<h1::Control<F, Err>, h1::ControlAck<F>, Rc<dyn Error>>,
    h2_ctl: Pipeline<h2::Control<H2Error>, h2::ControlAck, Rc<dyn Error>>,
    config: DispatcherConfig,
    _t: marker::PhantomData<(Hst, F, B)>,
}

impl<F, B, Err> HttpService<(), F, B, Err>
where
    F: Filter,
    B: MessageBody,
    Err: ResponseError + 'static,
{
    #[must_use]
    /// Create new `HttpService` instance.
    pub fn new<Sf>(sf: impl IntoServiceFactory<Sf, (), Request>) -> HttpService<(), F, B, Err>
    where
        Sf: ServiceFactory<Request, (), Error = Err, InitCfg = SharedCfg> + 'static,
        Sf::Res: Into<Response<B>>,
        Sf::InitError: Error,
    {
        HttpService {
            sf: PipelineFactory::new(sf.into_factory().map(Into::into).map_init_err(dyn_rc_err)),
            h1_ctl: Pipeline::new(h1::DefaultControlService::new()),
            h2_ctl: Pipeline::new(h2::DefaultControlService),
            config: DispatcherConfig::default(),
            _t: marker::PhantomData,
        }
    }
}

impl<F, B, Err> HttpService<(), F, B, Err>
where
    F: Filter,
    B: MessageBody,
    Err: ResponseError + 'static,
{
    #[must_use]
    /// Create *http service* for HTTP/1 protocol.
    pub fn h1<Sf>(sf: impl IntoServiceFactory<Sf, (), Request>) -> h1::H1Service<(), F, B, Err>
    where
        Sf: ServiceFactory<Request, Error = Err, InitCfg = SharedCfg> + 'static,
        Sf::Res: Into<Response<B>>,
        Sf::InitError: Error,
    {
        h1::H1Service::new(sf)
    }

    #[must_use]
    /// Create *http service* for HTTP/2 protocol.
    pub fn h2<Sf>(sf: impl IntoServiceFactory<Sf, (), Request>) -> h2::H2Service<(), F, B, Err>
    where
        Sf: ServiceFactory<Request, Error = Err, InitCfg = SharedCfg> + 'static,
        Sf::Res: Into<Response<B>>,
        Sf::InitError: Error,
    {
        h2::H2Service::new(sf)
    }
}

impl<Hst, F, B, Err> HttpService<Hst, F, B, Err>
where
    F: Filter,
    B: MessageBody,
    Err: ResponseError + 'static,
{
    #[must_use]
    /// Provide http/1 control service.
    pub fn h1_control<Ctl>(self, ctl: impl IntoService<Ctl, Hst>) -> HttpService<Hst, F, B, Err>
    where
        Hst: State<Ctl::Req>,
        Ctl: Service<Hst, Req = h1::Control<F, Err>, Res = h1::ControlAck<F>> + 'static,
        Ctl::Error: Error + 'static,
    {
        HttpService {
            sf: self.sf,
            config: self.config,
            h1_ctl: Pipeline::with(ctl.into_service().map_err(dyn_rc_err)),
            h2_ctl: self.h2_ctl,
            _t: marker::PhantomData,
        }
    }

    #[must_use]
    /// Provide http/1 control service.
    pub fn h2_control<Ctl>(self, ctl: impl IntoService<Ctl, Hst>) -> HttpService<Hst, F, B, Err>
    where
        Hst: State<Ctl::Req>,
        Ctl: Service<Hst, Req = h2::Control<H2Error>, Res = h2::ControlAck> + 'static,
        Ctl::Error: Error + 'static,
    {
        HttpService {
            sf: self.sf,
            config: self.config,
            h1_ctl: self.h1_ctl,
            h2_ctl: Pipeline::with(ctl.into_service().map_err(dyn_rc_err)),
            _t: marker::PhantomData,
        }
    }
}

impl<Hst, F, B, Err> Service<Hst> for HttpService<Hst, F, B, Err>
where
    F: Filter,
    B: MessageBody,
    Err: ResponseError + 'static,
{
    type Req = Io<F>;
    type Res = ();
    type Error = DispatchError;

    async fn ready(&self, _: ReadyCtx<'_, Self, Hst>) -> Result<(), Self::Error> {
        let (r1, r2) = join(self.h1_ctl.ready(), self.h2_ctl.ready()).await;
        r1.map_err(|e| {
            log::error!("Http control service readiness error: {e:?}");
            DispatchError::Control(e)
        })?;
        r2.map_err(|e| {
            log::error!("Http control service readiness error: {e:?}");
            DispatchError::Control(e)
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

    async fn call(&self, io: Io<F>, ctx: Ctx<'_, Self, Hst>) -> Result<Self::Res, Self::Error> {
        let cfg = io.shared();
        let svc = self.sf.create(&cfg, ctx.st()).await.map_err(|e| {
            log::error!("Cannot construct handler service: {e:?}");
            DispatchError::Control(e)
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
            self.config.notify_shutdown();
        }
        result
    }
}
