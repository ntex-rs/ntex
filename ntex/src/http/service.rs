use std::{cell::Cell, cell::RefCell, error, fmt, marker, rc::Rc};

use crate::io::{Filter, Io, IoRef, types};
use crate::service::{Ctx, IntoServiceFactory, ReadyCtx, Service, ServiceFactory};
use crate::{SharedCfg, channel::oneshot, util::HashSet, util::join};

use super::body::MessageBody;
use super::config::DispatcherConfig;
use super::error::{DispatchError, H2Error, ResponseError};
use super::request::Request;
use super::response::Response;
use super::{h1, h2};

/// `ServiceFactory` HTTP1.1/HTTP2 transport implementation
#[derive(derive_more::Debug)]
#[debug("HttpService")]
pub struct HttpService<
    F,
    Sf: ServiceFactory<Request>,
    B,
    C1 = h1::DefaultControlService<F, ()>,
    C2 = h2::DefaultControlService,
> {
    srv: Sf,
    h1_control: C1,
    h2_control: Rc<C2>,
    _t: marker::PhantomData<(F, B)>,
}

impl<F, Sf, B> HttpService<F, Sf, B, h1::DefaultControlService<F, Sf::Error>>
where
    F: Filter,
    Sf: ServiceFactory<Request, St = (), InitCfg = SharedCfg> + 'static,
    Sf::Res: Into<Response<B>>,
    Sf::Error: ResponseError,
    Sf::InitError: fmt::Debug,
    B: MessageBody,
{
    /// Create new `HttpService` instance.
    pub fn new(service: impl IntoServiceFactory<Sf, Request>) -> Self {
        HttpService {
            srv: service.into_factory(),
            h1_control: h1::DefaultControlService::new(),
            h2_control: Rc::new(h2::DefaultControlService),
            _t: marker::PhantomData,
        }
    }
}

impl<F, Sf, B> HttpService<F, Sf, B>
where
    F: Filter,
    Sf: ServiceFactory<Request, St = (), InitCfg = SharedCfg> + 'static,
    Sf::Res: Into<Response<B>>,
    Sf::Error: ResponseError,
    Sf::InitError: fmt::Debug,
    B: MessageBody,
{
    /// Create *http service* for HTTP/1 protocol.
    pub fn h1(
        sf: impl IntoServiceFactory<Sf, Request>,
    ) -> h1::H1Service<F, Sf, B, h1::DefaultControlService<F, Sf::Error>> {
        h1::H1Service::new(sf)
    }

    /// Create *http service* for HTTP/2 protocol.
    pub fn h2(
        sf: impl IntoServiceFactory<Sf, Request>,
    ) -> h2::H2Service<F, Sf, B, h2::DefaultControlService> {
        h2::H2Service::new(sf)
    }
}

impl<F, Sf, B, C1, C2> HttpService<F, Sf, B, C1, C2>
where
    F: Filter,
    Sf: ServiceFactory<Request, St = (), InitCfg = SharedCfg> + 'static,
    Sf::Res: Into<Response<B>>,
    Sf::Error: ResponseError,
    Sf::InitError: fmt::Debug,
    B: MessageBody,
    C1: ServiceFactory<
            h1::Control<F, Sf::Error>,
            St = (),
            Res = h1::ControlAck<F>,
            InitCfg = SharedCfg,
        >,
    C1::Error: error::Error,
    C1::InitError: fmt::Debug,
    C2: ServiceFactory<
            h2::Control<H2Error>,
            St = (),
            InitCfg = SharedCfg,
            Res = h2::ControlAck,
        >,
    C2::Error: error::Error,
    C2::InitError: fmt::Debug,
{
    /// Provide http/1 control service.
    pub fn h1_control<CT, U>(self, control: U) -> HttpService<F, Sf, B, CT, C2>
    where
        U: IntoServiceFactory<CT, h1::Control<F, Sf::Error>>,
        CT: ServiceFactory<
                h1::Control<F, Sf::Error>,
                St = (),
                Res = h1::ControlAck<F>,
                InitCfg = SharedCfg,
            >,
        CT::Error: error::Error,
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
    pub fn h2_control<CT, U>(self, control: U) -> HttpService<F, Sf, B, C1, CT>
    where
        U: IntoServiceFactory<CT, h2::Control<H2Error>>,
        CT: ServiceFactory<
                h2::Control<H2Error>,
                St = (),
                InitCfg = SharedCfg,
                Res = h2::ControlAck,
            >,
        CT::Error: error::Error,
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

    impl<F, Sf, B, C1, C2> HttpService<Layer<SslFilter, F>, Sf, B, C1, C2>
    where
        F: Filter,
        Sf: ServiceFactory<Request, St = (), InitCfg = SharedCfg> + 'static,
        Sf::Res: Into<Response<B>>,
        Sf::Error: ResponseError,
        Sf::InitError: fmt::Debug,
        B: MessageBody,
        C1: ServiceFactory<
                h1::Control<Layer<SslFilter, F>, Sf::Error>,
                St = (),
                Res = h1::ControlAck<Layer<SslFilter, F>>,
                InitCfg = SharedCfg,
            > + 'static,
        C1::Error: error::Error,
        C1::InitError: fmt::Debug,
        C2: ServiceFactory<
                h2::Control<H2Error>,
                St = (),
                InitCfg = SharedCfg,
                Res = h2::ControlAck,
            > + 'static,
        C2::Error: error::Error,
        C2::InitError: fmt::Debug,
    {
        /// Create openssl based service
        pub fn openssl(
            self,
            acceptor: ssl::SslAcceptor,
        ) -> impl ServiceFactory<
            Io<F>,
            St = (),
            Res = (),
            Error = SslError<DispatchError>,
            InitCfg = SharedCfg,
            InitError = (),
        > {
            SslAcceptor::new(acceptor)
                .map_err(SslError::Ssl)
                .map_init_err(|()| unreachable!())
                .and_then(self.map_err(SslError::Service))
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

    impl<F, Sf, B, C1, C2> HttpService<Layer<TlsServerFilter, F>, Sf, B, C1, C2>
    where
        F: Filter,
        Sf: ServiceFactory<Request, St = (), InitCfg = SharedCfg> + 'static,
        Sf::Res: Into<Response<B>>,
        Sf::Error: ResponseError,
        Sf::InitError: fmt::Debug,
        B: MessageBody,
        C1: ServiceFactory<
                h1::Control<Layer<TlsServerFilter, F>, Sf::Error>,
                St = (),
                Res = h1::ControlAck<Layer<TlsServerFilter, F>>,
                InitCfg = SharedCfg,
            > + 'static,
        C1::Error: error::Error,
        C1::InitError: fmt::Debug,
        C2: ServiceFactory<
                h2::Control<H2Error>,
                St = (),
                InitCfg = SharedCfg,
                Res = h2::ControlAck,
            > + 'static,
        C2::Error: error::Error,
        C2::InitError: fmt::Debug,
    {
        /// Create openssl based service
        pub fn rustls(
            self,
            mut config: ServerConfig,
        ) -> impl ServiceFactory<
            Io<F>,
            St = (),
            Res = (),
            Error = SslError<DispatchError>,
            InitCfg = SharedCfg,
            InitError = (),
        > {
            let protos = vec!["h2".to_string().into(), "http/1.1".to_string().into()];
            config.alpn_protocols = protos;

            TlsAcceptor::from(config)
                .map_err(|e| SslError::Ssl(Box::new(e)))
                .map_init_err(|()| unreachable!())
                .and_then(self.map_err(SslError::Service))
        }
    }
}

impl<F, Sf, B, C1, C2> ServiceFactory<Io<F>> for HttpService<F, Sf, B, C1, C2>
where
    F: Filter,
    Sf: ServiceFactory<Request, St = (), InitCfg = SharedCfg> + 'static,
    Sf::Res: Into<Response<B>>,
    Sf::Error: ResponseError,
    Sf::InitError: fmt::Debug,
    B: MessageBody,
    C1: ServiceFactory<
            h1::Control<F, Sf::Error>,
            St = (),
            Res = h1::ControlAck<F>,
            InitCfg = SharedCfg,
        > + 'static,
    C1::Error: error::Error,
    C1::InitError: fmt::Debug,
    C2: ServiceFactory<
            h2::Control<H2Error>,
            St = (),
            Res = h2::ControlAck,
            InitCfg = SharedCfg,
        > + 'static,
    C2::Error: error::Error,
    C2::InitError: fmt::Debug,
{
    type St = ();
    type Res = ();
    type Error = DispatchError;

    type Service = HttpServiceHandler<F, Sf::Service, B, C1::Service, C2>;
    type InitCfg = SharedCfg;
    type InitError = ();

    async fn create(&self, cfg: &SharedCfg) -> Result<Self::Service, Self::InitError> {
        let service = self
            .srv
            .create(cfg)
            .await
            .map_err(|e| log::error!("Cannot construct publish service: {e:?}"))?;
        let control = self
            .h1_control
            .create(cfg)
            .await
            .map_err(|e| log::error!("Cannot construct control service: {e:?}"))?;

        let (tx, rx) = oneshot::channel();
        let config = DispatcherConfig::new(cfg.get(), service, control);

        Ok(HttpServiceHandler {
            cfg: cfg.clone(),
            config: Rc::new(config),
            h2_control: self.h2_control.clone(),
            inflight: RefCell::new(HashSet::default()),
            rx: Cell::new(Some(rx)),
            tx: Cell::new(Some(tx)),
            _t: marker::PhantomData,
        })
    }
}

/// `Service` implementation for http transport
#[derive(derive_more::Debug)]
#[debug("HttpServiceHandler")]
pub struct HttpServiceHandler<F, S, B, C1, C2> {
    cfg: SharedCfg,
    config: Rc<DispatcherConfig<S, C1>>,
    h2_control: Rc<C2>,
    inflight: RefCell<HashSet<IoRef>>,
    rx: Cell<Option<oneshot::Receiver<()>>>,
    tx: Cell<Option<oneshot::Sender<()>>>,
    _t: marker::PhantomData<(F, B)>,
}

impl<F, S, B, C1, C2> Service for HttpServiceHandler<F, S, B, C1, C2>
where
    F: Filter,
    S: Service<St = (), Req = Request> + 'static,
    S::Res: Into<Response<B>>,
    S::Error: ResponseError,
    B: MessageBody,
    C1: Service<St = (), Req = h1::Control<F, S::Error>, Res = h1::ControlAck<F>> + 'static,
    C1::Error: error::Error,
    C2: ServiceFactory<
            h2::Control<H2Error>,
            St = (),
            Res = h2::ControlAck,
            InitCfg = SharedCfg,
        > + 'static,
    C2::Error: error::Error,
    C2::InitError: fmt::Debug,
{
    type St = ();
    type Req = Io<F>;
    type Res = ();
    type Error = DispatchError;

    async fn ready(&self, _: ReadyCtx<'_, Self>) -> Result<(), Self::Error> {
        let cfg = self.config.as_ref();

        let (ready1, ready2) = join(cfg.control.ready(&()), cfg.service.ready(&())).await;
        ready1.map_err(|e| {
            log::error!("Http control service readiness error: {e:?}");
            DispatchError::Control(Rc::new(e))
        })?;
        ready2.map_err(|e| {
            log::error!("Http service readiness error: {e:?}");
            DispatchError::Service(Rc::new(e))
        })
    }

    async fn shutdown(&self) {
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

    async fn call(&self, io: Io<F>, _: Ctx<'_, Self>) -> Result<Self::Res, Self::Error> {
        let id = self.config.next_id();
        let ioref = io.get_ref();

        let result = if io.query::<types::HttpProtocol>().get()
            == Some(types::HttpProtocol::Http2)
        {
            let control = self.h2_control.create(&self.cfg).await.map_err(|e| {
                DispatchError::Control(crate::util::str_rc_error(format!(
                    "Cannot construct control service: {e:?}"
                )))
            })?;
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
