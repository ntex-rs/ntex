use std::{collections::VecDeque, future::Future, io, marker, net::SocketAddr};

use ntex_error::Error;
use ntex_io::{Io, IoConfig, types};
use ntex_service::cfg::{Cfg, SharedCfg};
use ntex_service::{Ctx, Service, ServiceFactory};
use ntex_util::{future::Either, future::Ready, time::timeout_checked};

use super::{Address, Connect, ConnectError, ConnectServiceError, resolve};

#[derive(Copy, Clone, Debug)]
/// Basic tcp stream connector
pub struct Connector<T>(marker::PhantomData<T>);

#[derive(Clone, Debug)]
/// Basic tcp stream connector
pub struct ConnectorService<T, St = ()> {
    cfg: Cfg<IoConfig>,
    shared: SharedCfg,
    _t: marker::PhantomData<(T, St)>,
}

impl<T> Connector<T> {
    /// Construct new connect service with default configuration
    pub fn new() -> Self {
        Connector(marker::PhantomData)
    }
}

impl<T> Default for Connector<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, St> ConnectorService<T, St> {
    #[inline]
    /// Construct new connect service with default configuration
    pub fn new() -> Self {
        ConnectorService::with(SharedCfg::default())
    }

    #[inline]
    /// Construct new connect service with custom configuration
    pub fn with(cfg: SharedCfg) -> Self {
        ConnectorService {
            cfg: cfg.get(),
            shared: cfg,
            _t: marker::PhantomData,
        }
    }
}

impl<T, St> Default for ConnectorService<T, St> {
    fn default() -> Self {
        ConnectorService::new()
    }
}

impl<T: Address, St> ConnectorService<T, St> {
    /// Resolve and connect to remote host
    pub async fn connect<U>(&self, message: U) -> Result<Io, Error<ConnectError>>
    where
        Connect<T>: From<U>,
    {
        timeout_checked(self.cfg.connect_timeout(), async {
            // resolve first
            let msg = resolve::lookup(message.into(), self.shared.tag()).await?;

            let port = msg.port();
            let Connect { req, addr, .. } = msg;

            if let Some(addr) = addr {
                connect(req, port, addr, self.shared.clone()).await
            } else if let Some(addr) = req.addr() {
                connect(req, addr.port(), Either::Left(addr), self.shared.clone()).await
            } else {
                Err(Error::from(ConnectError::Unresolved))
            }
        })
        .await
        .map_err(|()| {
            Error::from(ConnectError::Io(io::Error::new(
                io::ErrorKind::TimedOut,
                "Connect timeout",
            )))
        })
        .and_then(|item| item)
        .map_err(|e| e.set_service(self.shared.service()))
    }
}

impl<T: Address, St> ServiceFactory<St, Connect<T>> for Connector<T> {
    type Res = Io;
    type Error = Error<ConnectError>;
    type Service = ConnectorService<T, St>;
    type InitCfg = SharedCfg;
    type InitError = ConnectServiceError;

    fn create(
        &self,
        cfg: &SharedCfg,
    ) -> impl Future<Output = Result<Self::Service, Self::InitError>> {
        Ready::Ok(ConnectorService::with(cfg.clone()))
    }
}

impl<T: Address, St> Service<St> for ConnectorService<T, St> {
    type Req = Connect<T>;
    type Res = Io;
    type Error = Error<ConnectError>;

    async fn call(&self, req: Connect<T>, _: Ctx<'_, Self, St>) -> Result<Io, Self::Error> {
        self.connect(req).await
    }
}

/// Tcp stream connector
async fn connect<T: Address>(
    req: T,
    port: u16,
    addr: Either<SocketAddr, VecDeque<SocketAddr>>,
    cfg: SharedCfg,
) -> Result<Io, Error<ConnectError>> {
    log::trace!(
        "{}: TCP connector - connecting to {:?} addr:{addr:?} port:{port}",
        cfg.tag(),
        req.host(),
    );

    let io = match addr {
        Either::Left(addr) => crate::tcp_connect(addr, cfg.clone())
            .await
            .map_err(ConnectError::from)?,
        Either::Right(mut addrs) => loop {
            let addr = addrs.pop_front().unwrap();

            match crate::tcp_connect(addr, cfg.clone()).await {
                Ok(io) => break io,
                Err(err) => {
                    log::trace!(
                        "{}: TCP connector - failed to connect to {:?} port: {port} err: {err:?}",
                        cfg.tag(),
                        req.host(),
                    );
                    if addrs.is_empty() {
                        return Err(ConnectError::from(err).into());
                    }
                }
            }
        },
    };

    log::trace!(
        "{}: TCP connector - successfully connected to {:?} - {:?}",
        cfg.tag(),
        req.host(),
        io.query::<types::PeerAddr>().get()
    );
    Ok(io)
}
