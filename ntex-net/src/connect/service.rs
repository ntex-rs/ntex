use std::{collections::VecDeque, io, marker, net::SocketAddr};

use ntex_error::Error;
use ntex_io::{Io, IoConfig, types};
use ntex_service::cfg::{Cfg, SharedCfg};
use ntex_service::{Ctx, Service, ServiceFactory};
use ntex_util::{future::Either, time::timeout_checked};

use super::{Address, Connect, ConnectError, ConnectServiceError, resolve};

#[derive(Copy, Clone, Debug)]
/// Basic tcp stream connector
pub struct Connector<A, St = ()>(marker::PhantomData<(A, St)>);

#[derive(Clone, Debug)]
/// Basic tcp stream connector
pub struct ConnectorService<A> {
    cfg: Cfg<IoConfig>,
    shared: SharedCfg,
    _t: marker::PhantomData<A>,
}

impl<A> Connector<A> {
    /// Construct new connect service with default configuration
    pub fn new() -> Self {
        Connector(marker::PhantomData)
    }
}

impl<A> Default for Connector<A> {
    fn default() -> Self {
        Self::new()
    }
}

impl<A> ConnectorService<A> {
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

impl<A> Default for ConnectorService<A> {
    fn default() -> Self {
        ConnectorService::new()
    }
}

impl<A: Address> ConnectorService<A> {
    /// Resolve and connect to remote host
    pub async fn connect<U>(&self, message: U) -> Result<Io, Error<ConnectError>>
    where
        Connect<A>: From<U>,
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

impl<A: Address, St> ServiceFactory<St, Connect<A>, SharedCfg> for Connector<A, St> {
    type Res = Io;
    type Error = Error<ConnectError>;

    type Service = ConnectorService<A>;
    type InitError = ConnectServiceError;

    async fn create(&self, cfg: &SharedCfg) -> Result<Self::Service, Self::InitError> {
        Ok(ConnectorService::with(cfg.clone()))
    }
}

impl<A: Address, St> Service<St, Connect<A>> for ConnectorService<A> {
    type Res = Io;
    type Error = Error<ConnectError>;

    async fn call(&self, req: Connect<A>, _: Ctx<'_, Self, St>) -> Result<Io, Self::Error> {
        self.connect(req).await
    }
}

/// Tcp stream connector
async fn connect<A: Address>(
    req: A,
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
