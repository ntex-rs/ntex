use std::{collections::VecDeque, io, marker, net::SocketAddr};

use ntex_error::Error;
use ntex_io::{Io, IoConfig, types};
use ntex_service::{Ctx, Service, cfg::Cfg, cfg::SharedCfg};
use ntex_util::{future::Either, time::timeout_checked};

use super::{Address, Connect, ConnectError, resolve};

#[derive(Debug)]
/// Basic tcp stream connector
pub struct Connector<A, St = ()> {
    cfg: Cfg<IoConfig>,
    shared: SharedCfg,
    _t: marker::PhantomData<(A, St)>,
}

impl<A> Connector<A> {
    #[inline]
    /// Construct new connect service with default configuration
    pub fn new() -> Self {
        Connector::with(SharedCfg::default())
    }

    #[inline]
    /// Construct new connect service with custom configuration
    pub fn with(cfg: impl Into<SharedCfg>) -> Self {
        let cfg = cfg.into();
        Connector {
            cfg: cfg.get(),
            shared: cfg,
            _t: marker::PhantomData,
        }
    }

    #[inline]
    /// Connector shared config
    pub fn cfg(&self) -> SharedCfg {
        self.cfg.shared()
    }
}

impl<A> Connector<A> {
    #[inline]
    /// Construct new connect service with custom configuration
    pub fn with_state<St>(cfg: impl Into<SharedCfg>) -> Connector<A, St> {
        let cfg = cfg.into();
        Connector {
            cfg: cfg.get(),
            shared: cfg,
            _t: marker::PhantomData,
        }
    }
}

impl<A> Default for Connector<A> {
    fn default() -> Self {
        Connector::new()
    }
}

impl<A, St> Clone for Connector<A, St> {
    fn clone(&self) -> Self {
        Connector {
            cfg: self.cfg.clone(),
            shared: self.shared.clone(),
            _t: marker::PhantomData,
        }
    }
}

impl<A: Address, St> Connector<A, St> {
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
                connect(req, port, addr, &self.shared).await
            } else if let Some(addr) = req.addr() {
                connect(req, addr.port(), Either::Left(addr), &self.shared).await
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

impl<A: Address, St> Service<St, Connect<A>> for Connector<A, St> {
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
    cfg: &SharedCfg,
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
