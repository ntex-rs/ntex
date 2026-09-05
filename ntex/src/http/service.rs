use crate::error::{Error, ErrorDiagnostic, ErrorInfo};
use crate::io::{Filter, Io, types};
use crate::service::{IntoServiceFactory, RequestState, pipeline::PipelineFactory};
use crate::{Ctx, Service, ServiceFactory};

use super::error::{DispatchError, H2Error, ResponseError};
use super::{Request, Response, config::DispatcherConfig, h1, h2};

/// HTTP1.1/HTTP2 transport implementation
#[derive(derive_more::Debug)]
#[debug("HttpService")]
pub struct HttpService<F, Req: RequestState<Io<F>>, Err> {
    sf: super::HttpPipeline<Req::State, Err>,
    h1_ctl: super::Ctl1Pipeline<Req::State, F, Err>,
    h2_ctl: super::Ctl2Pipeline<Req::State>,
    config: DispatcherConfig,
}

impl<F, Req, Err> HttpService<F, Req, Err>
where
    F: Filter + 'static,
    Req: RequestState<Io<F>>,
    Req::State: Clone,
    Err: ResponseError + 'static,
{
    #[must_use]
    /// Create new `HttpService` instance.
    pub fn new<H>(sf: impl IntoServiceFactory<H, Req::State, Request>) -> Self
    where
        H: ServiceFactory<Req::State, Request, Error = Err> + 'static,
        H::Res: Into<Response>,
        H::InitError: ErrorDiagnostic,
    {
        HttpService {
            sf: PipelineFactory::new(
                sf.into_factory()
                    .map(Into::into)
                    .map_init_err(|e| DispatchError::Control(ErrorInfo::from(Error::from(e)))),
            ),
            h1_ctl: PipelineFactory::new(h1::DefaultControlService),
            h2_ctl: PipelineFactory::new(h2::DefaultControlService),
            config: DispatcherConfig::default(),
        }
    }

    #[must_use]
    /// Create *http service* for HTTP/1 protocol.
    pub fn h1<H>(sf: impl IntoServiceFactory<H, Req::State, Request>) -> h1::H1Service<F, Req, Err>
    where
        H: ServiceFactory<Req::State, Request, Error = Err> + 'static,
        H::Res: Into<Response>,
        H::InitError: ErrorDiagnostic,
    {
        h1::H1Service::new(sf)
    }

    #[must_use]
    /// Create *http service* for HTTP/2 protocol.
    pub fn h2<H>(sf: impl IntoServiceFactory<H, Req::State, Request>) -> h2::H2Service<F, Req, Err>
    where
        H: ServiceFactory<Req::State, Request, Error = Err> + 'static,
        H::Res: Into<Response>,
        H::InitError: ErrorDiagnostic,
    {
        h2::H2Service::new(sf)
    }
}

impl<F, Req, Err> HttpService<F, Req, Err>
where
    F: Filter,
    Req: RequestState<Io<F>>,
    Err: ResponseError + 'static,
{
    #[must_use]
    /// Provide http/1 control service.
    pub fn h1_control<Ctl>(
        self,
        ctl: impl IntoServiceFactory<Ctl, Req::State, h1::Control<F, Err>>,
    ) -> Self
    where
        Ctl: ServiceFactory<Req::State, h1::Control<F, Err>, Res = h1::ControlAck<F>> + 'static,
        Ctl::Error: ErrorDiagnostic,
        Ctl::InitError: ErrorDiagnostic,
    {
        HttpService {
            sf: self.sf,
            h1_ctl: PipelineFactory::new(
                ctl.into_factory()
                    .map_err(|e| DispatchError::Service(ErrorInfo::from(Error::from(e))))
                    .map_init_err(|e| DispatchError::Control(ErrorInfo::from(Error::from(e)))),
            ),
            h2_ctl: self.h2_ctl,
            config: self.config,
        }
    }

    #[must_use]
    /// Provide http/1 control service.
    pub fn h2_control<Ctl>(
        self,
        ctl: impl IntoServiceFactory<Ctl, Req::State, h2::Control<H2Error>>,
    ) -> Self
    where
        Ctl: ServiceFactory<Req::State, h2::Control<H2Error>, Res = h2::ControlAck> + 'static,
        Ctl::Error: ErrorDiagnostic,
        Ctl::InitError: ErrorDiagnostic,
    {
        HttpService {
            sf: self.sf,
            h1_ctl: self.h1_ctl,
            h2_ctl: PipelineFactory::new(
                ctl.into_factory()
                    .map_err(|e| DispatchError::Service(ErrorInfo::from(Error::from(e))))
                    .map_init_err(|e| DispatchError::Control(ErrorInfo::from(Error::from(e)))),
            ),
            config: self.config,
        }
    }
}

impl<St, F, Req, Err> Service<St, Req> for HttpService<F, Req, Err>
where
    F: Filter + 'static,
    Req: RequestState<Io<F>>,
    Req::State: Clone,
    Err: ResponseError + 'static,
{
    type Res = ();
    type Error = DispatchError;

    async fn call(&self, req: Req, _: Ctx<'_, Self, St>) -> Result<Self::Res, Self::Error> {
        let (st, io) = req.unpack();

        let svc = self.sf.create(st.clone()).await?;

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
            let ctl = self.h2_ctl.create(st).await?;

            h2::handle(id, io.into(), svc, ctl).await
        } else {
            log::trace!(
                "{}: New http1 connection {id}, peer address {:?}, in-flight: {inflight}",
                io.tag(),
                io.query::<types::PeerAddr>().get(),
            );
            let ctl = self.h1_ctl.create(st).await?;

            h1::handle_io(id, io, svc, ctl, self.config.clone()).await
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
