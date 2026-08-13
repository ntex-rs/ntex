use std::{cell::Cell, cell::RefCell, error::Error, fmt, marker, rc::Rc, task::Context};

use crate::http::body::MessageBody;
use crate::http::config::DispatcherConfig;
use crate::http::error::{DispatchError, ResponseError};
use crate::http::{request::Request, response::Response};
use crate::io::{Filter, Io, IoRef, types};
use crate::service::{Ctx, IntoServiceFactory, ReadyCtx, Service, ServiceFactory};
use crate::{SharedCfg, channel::oneshot, util::HashSet, util::join};

use super::control::{Control, ControlAck, ControlResult};
use super::default::DefaultControlService;
use super::dispatcher::Dispatcher;

/// `ServiceFactory` implementation for HTTP1 transport
#[derive(derive_more::Debug)]
#[debug("H1Service")]
pub struct H1Service<F, Sf, B, C> {
    srv: Sf,
    ctl: C,
    _t: marker::PhantomData<(F, B)>,
}

impl<F, Sf, B> H1Service<F, Sf, B, DefaultControlService>
where
    Sf: ServiceFactory<(), Request, InitCfg = SharedCfg> + 'static,
    Sf::Res: Into<Response<B>>,
    Sf::Error: ResponseError,
    Sf::InitError: fmt::Debug,
    B: MessageBody,
{
    /// Create new `HttpService` instance with config.
    pub(crate) fn new<U: IntoServiceFactory<Sf, (), Request>>(service: U) -> Self {
        H1Service {
            srv: service.into_factory(),
            ctl: DefaultControlService,
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

    impl<F, Sf, B, C> H1Service<Layer<SslFilter, F>, Sf, B, C>
    where
        F: Filter,
        Sf: ServiceFactory<(), Request, InitCfg = SharedCfg> + 'static,
        Sf::Res: Into<Response<B>>,
        Sf::Error: ResponseError,
        Sf::InitError: fmt::Debug,
        B: MessageBody,
        C: ServiceFactory<
                (),
                Control<Layer<SslFilter, F>, Sf::Error>,
                Res = ControlAck<Layer<SslFilter, F>>,
                InitCfg = SharedCfg,
            > + 'static,
        C::Error: Error,
        C::InitError: fmt::Debug,
    {
        /// Create openssl based service
        pub fn openssl(
            self,
            acceptor: ssl::SslAcceptor,
        ) -> impl ServiceFactory<
            (),
            Io<F>,
            Res = (),
            Error = SslError<DispatchError>,
            InitCfg = SharedCfg,
            InitError = (),
        > {
            SslAcceptor::new(acceptor)
                .map_err(SslError::Ssl)
                .map_init_err(|()| panic!())
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

    impl<F, Sf, B, C> H1Service<Layer<TlsServerFilter, F>, Sf, B, C>
    where
        F: Filter,
        Sf: ServiceFactory<(), Request, InitCfg = SharedCfg> + 'static,
        Sf::Res: Into<Response<B>>,
        Sf::Error: ResponseError,
        Sf::InitError: fmt::Debug,
        B: MessageBody,
        C: ServiceFactory<
                (),
                Control<Layer<TlsServerFilter, F>, Sf::Error>,
                Res = ControlAck<Layer<TlsServerFilter, F>>,
                InitCfg = SharedCfg,
            > + 'static,
        C::Error: Error,
        C::InitError: fmt::Debug,
    {
        /// Create rustls based service
        pub fn rustls(
            self,
            config: ServerConfig,
        ) -> impl ServiceFactory<
            (),
            Io<F>,
            Res = (),
            Error = SslError<DispatchError>,
            InitCfg = SharedCfg,
            InitError = (),
        > {
            TlsAcceptor::from(config)
                .map_err(|e| SslError::Ssl(Box::new(e)))
                .map_init_err(|()| panic!())
                .and_then(self.map_err(SslError::Service))
        }
    }
}

impl<F, Sf, B, C> H1Service<F, Sf, B, C>
where
    F: Filter,
    Sf: ServiceFactory<(), Request, InitCfg = SharedCfg>,
    Sf::Res: Into<Response<B>>,
    Sf::Error: ResponseError,
    Sf::InitError: fmt::Debug,
    B: MessageBody,
    C: ServiceFactory<(), Control<F, Sf::Error>, Res = ControlAck<F>, InitCfg = SharedCfg>,
    C::Error: Error,
    C::InitError: fmt::Debug,
{
    /// Provide http/1 control service
    pub fn control<C1, U>(self, ctl: U) -> H1Service<F, Sf, B, C1>
    where
        U: IntoServiceFactory<C1, (), Control<F, Sf::Error>>,
        C1: ServiceFactory<
                (),
                Control<F, Sf::Error>,
                Res = ControlAck<F>,
                InitCfg = SharedCfg,
            >,
        C1::Error: Error,
        C1::InitError: fmt::Debug,
    {
        H1Service {
            ctl: ctl.into_factory(),
            srv: self.srv,
            _t: marker::PhantomData,
        }
    }
}

impl<F, Sf, B, C> ServiceFactory<(), Io<F>> for H1Service<F, Sf, B, C>
where
    F: Filter,
    Sf: ServiceFactory<(), Request, InitCfg = SharedCfg> + 'static,
    Sf::Res: Into<Response<B>>,
    Sf::Error: ResponseError + 'static,
    Sf::InitError: fmt::Debug,
    Sf::Service: 'static,
    B: MessageBody,
    C: ServiceFactory<(), Control<F, Sf::Error>, InitCfg = SharedCfg, Res = ControlAck<F>>
        + 'static,
    C::Error: Error + 'static,
    C::InitError: fmt::Debug,
    C::Service: 'static,
{
    type Res = ();
    type Error = DispatchError;

    type InitCfg = SharedCfg;
    type InitError = ();
    type Service = H1ServiceHandler<F, Sf::Service, B, C::Service>;

    async fn create(&self, cfg: &SharedCfg) -> Result<Self::Service, Self::InitError> {
        let service = self
            .srv
            .create(cfg)
            .await
            .map_err(|e| log::error!("Cannot construct publish service: {e:?}"))?;
        let control = self
            .ctl
            .create(cfg)
            .await
            .map_err(|e| log::error!("Cannot construct control service: {e:?}"))?;

        let (tx, rx) = oneshot::channel();
        let config = Rc::new(DispatcherConfig::new(cfg.get(), service, control));

        Ok(H1ServiceHandler {
            config,
            inflight: RefCell::new(HashSet::default()),
            rx: Cell::new(Some(rx)),
            tx: Cell::new(Some(tx)),
            _t: marker::PhantomData,
        })
    }
}

/// `Service` implementation for HTTP1 transport
#[derive(derive_more::Debug)]
#[debug("H1ServiceHandler")]
pub struct H1ServiceHandler<F, S, B, C> {
    config: Rc<DispatcherConfig<S, C>>,
    inflight: RefCell<HashSet<IoRef>>,
    rx: Cell<Option<oneshot::Receiver<()>>>,
    tx: Cell<Option<oneshot::Sender<()>>>,
    _t: marker::PhantomData<(F, B)>,
}

impl<F, S, B, C> Service<(), Io<F>> for H1ServiceHandler<F, S, B, C>
where
    F: Filter,
    C: Service<(), Control<F, S::Error>, Res = ControlAck<F>> + 'static,
    C::Error: Error + 'static,
    S: Service<(), Request> + 'static,
    S::Res: Into<Response<B>>,
    S::Error: ResponseError + 'static,
    B: MessageBody,
{
    type Res = ();
    type Error = DispatchError;

    async fn ready(&self, _: ReadyCtx<'_, Self, ()>) -> Result<(), Self::Error> {
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

    fn poll(&self, cx: &mut Context<'_>) -> Result<(), Self::Error> {
        let cfg = self.config.as_ref();
        cfg.control
            .poll(cx)
            .map_err(|e| DispatchError::Control(Rc::new(e)))?;
        cfg.service
            .poll(cx)
            .map_err(|e| DispatchError::Service(Rc::new(e)))
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

    async fn call(&self, io: Io<F>, _: Ctx<'_, Self, ()>) -> Result<(), Self::Error> {
        let id = self.config.next_id();
        let inflight = {
            let mut inflight = self.inflight.borrow_mut();
            inflight.insert(io.get_ref());
            inflight.len()
        };
        let ioref = io.get_ref();

        log::trace!(
            "{}: New http1 connection {id}, peer address {:?}, inflight: {}",
            io.tag(),
            io.query::<types::PeerAddr>().get(),
            inflight
        );

        let result = handle_io(id, io, self.config.clone()).await;
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

pub(crate) async fn handle_io<F, S, B, C>(
    id: usize,
    io: Io<F>,
    config: Rc<DispatcherConfig<S, C>>,
) -> Result<(), DispatchError>
where
    F: Filter,
    C: Service<(), Control<F, S::Error>, Res = ControlAck<F>> + 'static,
    C::Error: Error,
    S: Service<(), Request> + 'static,
    S::Error: ResponseError,
    S::Res: Into<Response<B>>,
    B: MessageBody,
{
    // Notify control service
    let ack = config
        .control
        .call_nowait(Control::connect(id, io), ())
        .await;

    match ack {
        Ok(ack) => {
            let ControlResult::Connect(io) = ack.result else {
                unreachable!();
            };

            Dispatcher::new(id, io, config)
                .await
                .map_err(DispatchError::Control)
        }
        Err(e) => Err(DispatchError::Control(Rc::new(e))),
    }
}
