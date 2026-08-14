use std::{cell::Cell, cell::RefCell, error, fmt, marker, rc::Rc, task::Context};

use crate::io::{Filter, Io, IoRef, types};
use crate::service::{IntoServiceFactory, Service, ServiceCtx, ServiceFactory};
use crate::{SharedCfg, channel::oneshot, util::HashSet, util::join};

use super::body::MessageBody;
use super::config::DispatcherConfig;
use super::error::{DispatchError, H2Error, ResponseError};
use super::request::Request;
use super::response::Response;
use super::{h1, h2};

type FactoryError<T, R> = <T as ServiceFactory<R, SharedCfg>>::Error;

/// `ServiceFactory` HTTP1.1/HTTP2 transport implementation
#[derive(derive_more::Debug)]
#[debug("HttpService")]
pub struct HttpService<
    F,
    S,
    B,
    C1 = h1::DefaultControlService,
    C2 = h2::DefaultControlService,
> {
    srv: S,
    h1_control: C1,
    h2_control: Rc<C2>,
    _t: marker::PhantomData<(F, B)>,
}

impl<F, S, B> HttpService<F, S, B>
where
    F: Filter,
    S: ServiceFactory<Request, SharedCfg, Data = ()> + 'static,
    S::Service: Service<Request>,
    FactoryError<S, Request>: ResponseError,
    S::Response: Into<Response<B>>,
    S::InitError: fmt::Debug,
    B: MessageBody,
{
    /// Create new `HttpService` instance.
    pub fn new<U>(service: U) -> Self
    where
        U: IntoServiceFactory<S, Request, SharedCfg>,
    {
        HttpService {
            srv: service.into_factory(),
            h1_control: h1::DefaultControlService,
            h2_control: Rc::new(h2::DefaultControlService),
            _t: marker::PhantomData,
        }
    }
}

impl<F, S, B> HttpService<F, S, B>
where
    F: Filter,
    S: ServiceFactory<Request, SharedCfg, Data = ()> + 'static,
    S::Service: Service<Request>,
    FactoryError<S, Request>: ResponseError,
    S::Response: Into<Response<B>>,
    S::InitError: fmt::Debug,
    B: MessageBody,
{
    /// Create *http service* for HTTP/1 protocol.
    pub fn h1<U: IntoServiceFactory<S, Request, SharedCfg>>(
        service: U,
    ) -> h1::H1Service<F, S, B, h1::DefaultControlService> {
        h1::H1Service::new(service)
    }

    /// Create *http service* for HTTP/2 protocol.
    pub fn h2<U: IntoServiceFactory<S, Request, SharedCfg>>(
        service: U,
    ) -> h2::H2Service<F, S, B, h2::DefaultControlService> {
        h2::H2Service::new(service)
    }
}

impl<F, S, B, C1, C2> HttpService<F, S, B, C1, C2>
where
    F: Filter,
    S: ServiceFactory<Request, SharedCfg, Data = ()> + 'static,
    S::Service: Service<Request>,
    FactoryError<S, Request>: ResponseError,
    S::Response: Into<Response<B>>,
    S::InitError: fmt::Debug,
    B: MessageBody,
    C1: ServiceFactory<h1::Control<F, FactoryError<S, Request>>, SharedCfg, Data = ()>,
    C1::Service:
        Service<h1::Control<F, FactoryError<S, Request>>, Response = h1::ControlAck<F>>,
    <C1::Service as Service<h1::Control<F, FactoryError<S, Request>>>>::Error: error::Error,
    C1::InitError: fmt::Debug,
    C2: ServiceFactory<h2::Control<H2Error>, SharedCfg, Data = ()>,
    C2::Service: Service<h2::Control<H2Error>, Response = h2::ControlAck>,
    <C2::Service as Service<h2::Control<H2Error>>>::Error: error::Error,
    C2::InitError: fmt::Debug,
{
    /// Provide http/1 control service.
    pub fn h1_control<CT, U>(self, control: U) -> HttpService<F, S, B, CT, C2>
    where
        U: IntoServiceFactory<CT, h1::Control<F, FactoryError<S, Request>>, SharedCfg>,
        CT: ServiceFactory<h1::Control<F, FactoryError<S, Request>>, SharedCfg, Data = ()>,
        CT::Service:
            Service<h1::Control<F, FactoryError<S, Request>>, Response = h1::ControlAck<F>>,
        <CT::Service as Service<h1::Control<F, FactoryError<S, Request>>>>::Error:
            error::Error,
        CT::InitError: fmt::Debug,
    {
        HttpService {
            h1_control: control.into_factory(),
            h2_control: self.h2_control,
            srv: self.srv,
            _t: marker::PhantomData,
        }
    }

    /// Provide http/1 control service.
    pub fn h2_control<CT, U>(self, control: U) -> HttpService<F, S, B, C1, CT>
    where
        U: IntoServiceFactory<CT, h2::Control<H2Error>, SharedCfg>,
        CT: ServiceFactory<h2::Control<H2Error>, SharedCfg, Data = ()>,
        CT::Service: Service<h2::Control<H2Error>, Response = h2::ControlAck>,
        <CT::Service as Service<h2::Control<H2Error>>>::Error: error::Error,
        CT::InitError: fmt::Debug,
    {
        HttpService {
            h1_control: self.h1_control,
            h2_control: Rc::new(control.into_factory()),
            srv: self.srv,
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

    impl<F, S, B, C1, C2> HttpService<Layer<SslFilter, F>, S, B, C1, C2>
    where
        F: Filter,
        S: ServiceFactory<Request, SharedCfg, Data = ()> + 'static,
        S::Service: Service<Request>,
        FactoryError<S, Request>: ResponseError,
        S::Response: Into<Response<B>>,
        S::InitError: fmt::Debug,
        B: MessageBody,
        C1: ServiceFactory<
                h1::Control<Layer<SslFilter, F>, FactoryError<S, Request>>,
                SharedCfg,
                Data = (),
            > + 'static,
        C1::Service: Service<
                h1::Control<Layer<SslFilter, F>, FactoryError<S, Request>>,
                Response = h1::ControlAck<Layer<SslFilter, F>>,
            >,
        <C1::Service as Service<
            h1::Control<Layer<SslFilter, F>, FactoryError<S, Request>>,
        >>::Error: error::Error,
        C1::InitError: fmt::Debug,
        C2: ServiceFactory<h2::Control<H2Error>, SharedCfg, Data = ()> + 'static,
        C2::Service: Service<h2::Control<H2Error>, Response = h2::ControlAck>,
        <C2::Service as Service<h2::Control<H2Error>>>::Error: error::Error,
        C2::InitError: fmt::Debug,
    {
        /// Create openssl based service
        pub fn openssl(
            self,
            acceptor: ssl::SslAcceptor,
        ) -> impl ServiceFactory<
            Io<F>,
            SharedCfg,
            Data = (),
            Response = (),
            Error = SslError<DispatchError>,
            InitError = (),
        > {
            crate::service::chain_factory(SslAcceptor::new(acceptor))
                .map_err(SslError::Ssl)
                .map_init_err(|()| unreachable!())
                .and_then(crate::service::chain_factory(self).map_err(SslError::Service))
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

    impl<F, S, B, C1, C2> HttpService<Layer<TlsServerFilter, F>, S, B, C1, C2>
    where
        F: Filter,
        S: ServiceFactory<Request, SharedCfg, Data = ()> + 'static,
        S::Service: Service<Request>,
        FactoryError<S, Request>: ResponseError,
        S::Response: Into<Response<B>>,
        S::InitError: fmt::Debug,
        B: MessageBody,
        C1: ServiceFactory<
                h1::Control<Layer<TlsServerFilter, F>, FactoryError<S, Request>>,
                SharedCfg,
                Data = (),
            > + 'static,
        C1::Service: Service<
                h1::Control<Layer<TlsServerFilter, F>, FactoryError<S, Request>>,
                Response = h1::ControlAck<Layer<TlsServerFilter, F>>,
            >,
        <C1::Service as Service<
            h1::Control<Layer<TlsServerFilter, F>, FactoryError<S, Request>>,
        >>::Error: error::Error,
        C1::InitError: fmt::Debug,
        C2: ServiceFactory<h2::Control<H2Error>, SharedCfg, Data = ()> + 'static,
        C2::Service: Service<h2::Control<H2Error>, Response = h2::ControlAck>,
        <C2::Service as Service<h2::Control<H2Error>>>::Error: error::Error,
        C2::InitError: fmt::Debug,
    {
        /// Create openssl based service
        pub fn rustls(
            self,
            mut config: ServerConfig,
        ) -> impl ServiceFactory<
            Io<F>,
            SharedCfg,
            Data = (),
            Response = (),
            Error = SslError<DispatchError>,
            InitError = (),
        > {
            let protos = vec!["h2".to_string().into(), "http/1.1".to_string().into()];
            config.alpn_protocols = protos;

            crate::service::chain_factory(TlsAcceptor::from(config))
                .map_err(|e| SslError::Ssl(Box::new(e)))
                .map_init_err(|()| unreachable!())
                .and_then(crate::service::chain_factory(self).map_err(SslError::Service))
        }
    }
}

impl<F, S, B, C1, C2> ServiceFactory<Io<F>, SharedCfg> for HttpService<F, S, B, C1, C2>
where
    F: Filter,
    S: ServiceFactory<Request, SharedCfg, Data = ()> + 'static,
    S::Service: Service<Request>,
    FactoryError<S, Request>: ResponseError,
    S::Response: Into<Response<B>>,
    S::InitError: fmt::Debug,
    B: MessageBody,
    C1: ServiceFactory<h1::Control<F, FactoryError<S, Request>>, SharedCfg, Data = ()>
        + 'static,
    C1::Service:
        Service<h1::Control<F, FactoryError<S, Request>>, Response = h1::ControlAck<F>>,
    <C1::Service as Service<h1::Control<F, FactoryError<S, Request>>>>::Error: error::Error,
    C1::InitError: fmt::Debug,
    C2: ServiceFactory<h2::Control<H2Error>, SharedCfg, Data = ()> + 'static,
    C2::Service: Service<h2::Control<H2Error>, Response = h2::ControlAck>,
    <C2::Service as Service<h2::Control<H2Error>>>::Error: error::Error,
    C2::InitError: fmt::Debug,
{
    type Response = ();
    type Error = DispatchError;
    type Service = HttpServiceHandler<F, S::Service, B, C1::Service, C2>;
    type InitError = ();
    type Data = ();

    async fn create(&self, cfg: SharedCfg) -> Result<Self::Service, Self::InitError> {
        let service_data = self.srv.map_data(&cfg, &()).await.map_err(|e| {
            log::error!("Cannot construct publish service data: {e:?}");
        })?;
        let service = self.srv.create(cfg.clone()).await.map_err(|e| {
            log::error!("Cannot construct publish service: {e:?}");
        })?;
        let control_data = self.h1_control.map_data(&cfg, &()).await.map_err(|e| {
            log::error!("Cannot construct control service data: {e:?}");
        })?;
        let control = self.h1_control.create(cfg.clone()).await.map_err(|e| {
            log::error!("Cannot construct control service: {e:?}");
        })?;

        let (tx, rx) = oneshot::channel();
        let config =
            DispatcherConfig::new(cfg.get(), service, service_data, control, control_data);

        Ok(HttpServiceHandler {
            cfg,
            config: Rc::new(config),
            h2_control: self.h2_control.clone(),
            inflight: RefCell::new(HashSet::default()),
            rx: Cell::new(Some(rx)),
            tx: Cell::new(Some(tx)),
            _t: marker::PhantomData,
        })
    }

    async fn map_data(&self, _: &SharedCfg, _: &Self::Data) -> Result<(), Self::InitError> {
        Ok(())
    }
}

/// `Service` implementation for http transport
#[derive(derive_more::Debug)]
#[debug("HttpServiceHandler")]
pub struct HttpServiceHandler<F, S, B, C1, C2>
where
    S: Service<Request>,
    C1: Service<h1::Control<F, S::Error>>,
{
    cfg: SharedCfg,
    config: Rc<DispatcherConfig<S, C1, S::Data, C1::Data>>,
    h2_control: Rc<C2>,
    inflight: RefCell<HashSet<IoRef>>,
    rx: Cell<Option<oneshot::Receiver<()>>>,
    tx: Cell<Option<oneshot::Sender<()>>>,
    _t: marker::PhantomData<(F, B)>,
}

impl<F, S, B, C1, C2> Service<Io<F>> for HttpServiceHandler<F, S, B, C1, C2>
where
    F: Filter,
    S: Service<Request> + 'static,
    S::Error: ResponseError,
    S::Response: Into<Response<B>>,
    B: MessageBody,
    C1: Service<h1::Control<F, S::Error>, Response = h1::ControlAck<F>> + 'static,
    C1::Error: error::Error,
    C2: ServiceFactory<h2::Control<H2Error>, SharedCfg, Data = ()> + 'static,
    C2::Service: Service<h2::Control<H2Error>, Response = h2::ControlAck>,
    <C2::Service as Service<h2::Control<H2Error>>>::Error: error::Error,
    C2::InitError: fmt::Debug,
{
    type Response = ();
    type Error = DispatchError;
    type Data = ();

    async fn ready(
        &self,
        _: &Self::Data,
        _: ServiceCtx<'_, Self>,
    ) -> Result<(), Self::Error> {
        let cfg = self.config.as_ref();

        let (ready1, ready2) = join(cfg.control.ready(), cfg.service.ready()).await;
        ready1.map_err(|e| {
            log::error!("Http control service readiness error: {e:?}");
            DispatchError::Control(Rc::new(e))
        })?;
        ready2.map_err(|e| {
            log::error!("Http service readiness error: {e:?}");
            DispatchError::Service(Rc::new(e))
        })
    }

    #[inline]
    fn poll(&self, _: &Self::Data, cx: &mut Context<'_>) -> Result<(), Self::Error> {
        let cfg = self.config.as_ref();
        cfg.control
            .poll(cx)
            .map_err(|e| DispatchError::Control(Rc::new(e)))?;
        cfg.service
            .poll(cx)
            .map_err(|e| DispatchError::Service(Rc::new(e)))
    }

    async fn shutdown(&self, _: &Self::Data) {
        self.config.shutdown();

        // check inflight connections
        let inflight = {
            let inflight = self.inflight.borrow();
            for io in inflight.iter() {
                io.notify_dispatcher();
            }
            inflight.len()
        };
        if inflight != 0 {
            log::trace!("Shutting down service, in-flight connections: {inflight}");

            if let Some(rx) = self.rx.take() {
                let _ = rx.await;
            }

            log::trace!("Shutting down is complected");
        }

        join(
            self.config.control.shutdown(),
            self.config.service.shutdown(),
        )
        .await;
    }

    async fn call(
        &self,
        io: Io<F>,
        _: &Self::Data,
        _: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        let id = self.config.next_id();
        let ioref = io.get_ref();

        let result = if io.query::<types::HttpProtocol>().get()
            == Some(types::HttpProtocol::Http2)
        {
            let control_data =
                self.h2_control
                    .map_data(&self.cfg, &())
                    .await
                    .map_err(|e| {
                        DispatchError::Control(crate::util::str_rc_error(format!(
                            "Cannot construct control service data: {e:?}"
                        )))
                    })?;
            let control = self
                .h2_control
                .create(self.cfg.clone())
                .await
                .map_err(|e| {
                    DispatchError::Control(crate::util::str_rc_error(format!(
                        "Cannot construct control service: {e:?}"
                    )))
                })?;
            let control = h2::ControlService::new(control, control_data);
            let inflight = {
                let mut inflight = self.inflight.borrow_mut();
                inflight.insert(io.get_ref());
                inflight.len()
            };

            log::trace!(
                "{}: New http2 connection {id}, peer address {:?}, in-flight: {inflight}",
                io.tag(),
                io.query::<types::PeerAddr>().get(),
            );

            h2::handle(id, io.into(), control, self.config.clone()).await
        } else {
            let inflight = {
                let mut inflight = self.inflight.borrow_mut();
                inflight.insert(io.get_ref());
                inflight.len()
            };

            log::trace!(
                "{}: New http1 connection {id}, peer address {:?}, in-flight: {inflight}",
                io.tag(),
                io.query::<types::PeerAddr>().get(),
            );
            h1::handle_io(id, io, self.config.clone()).await
        };

        {
            let mut inflight = self.inflight.borrow_mut();
            inflight.remove(&ioref);

            if inflight.is_empty()
                && let Some(tx) = self.tx.take()
            {
                let _ = tx.send(());
            }
        }

        result
    }
}
