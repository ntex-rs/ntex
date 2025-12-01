use std::{collections::VecDeque, io, marker::PhantomData, net::SocketAddr};

use ntex_io::{Io, IoConfig, types};
use ntex_service::cfg::{Cfg, SharedCfg};
use ntex_service::{Service, ServiceCtx, ServiceFactory};
use ntex_util::{future::Either, time::timeout_checked};

use super::{Address, Connect, ConnectError, ConnectServiceError, Resolver};
use crate::tcp_connect;

#[derive(Copy, Clone, Debug)]
/// Basic tcp stream connector
pub struct Connector<T>(PhantomData<T>);

#[derive(Copy, Clone, Debug)]
/// Basic tcp stream connector
pub struct ConnectorService<T> {
    cfg: Cfg<IoConfig>,
    shared: SharedCfg,
    resolver: Resolver<T>,
}

impl<T> Connector<T> {
    /// Construct new connect service with default configuration
    pub fn new() -> Self {
        Connector(PhantomData)
    }
}

impl<T> Default for Connector<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> ConnectorService<T> {
    /// Construct new connect service with default configuration
    pub fn new() -> Self {
        ConnectorService {
            cfg: Default::default(),
            shared: Default::default(),
            resolver: Resolver::new(),
        }
    }

    /// Construct new connect service with custom configuration
    pub fn with(cfg: SharedCfg) -> Self {
        ConnectorService {
            cfg: cfg.get(),
            shared: cfg,
            resolver: Resolver::new(),
        }
    }
}

impl<T> Default for ConnectorService<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Address> ConnectorService<T> {
    /// Resolve and connect to remote host
    pub async fn connect<U>(&self, message: U) -> Result<Io, ConnectError>
    where
        Connect<T>: From<U>,
    {
        timeout_checked(self.cfg.connect_timeout(), async {
            // resolve first
            let address = self
                .resolver
                .lookup_with_tag(message.into(), self.shared.tag())
                .await?;

            let port = address.port();
            let Connect { req, addr, .. } = address;

            if let Some(addr) = addr {
                connect(req, port, addr, self.shared).await
            } else if let Some(addr) = req.addr() {
                connect(req, addr.port(), Either::Left(addr), self.shared).await
            } else {
                log::error!("{}: TCP connector: got unresolved address", self.cfg.tag());
                Err(ConnectError::Unresolved)
            }
        })
        .await
        .map_err(|_| {
            ConnectError::Io(io::Error::new(io::ErrorKind::TimedOut, "Connect timeout"))
        })
        .and_then(|item| item)
    }
}

impl<T: Address> ServiceFactory<Connect<T>, SharedCfg> for Connector<T> {
    type Response = Io;
    type Error = ConnectError;
    type Service = ConnectorService<T>;
    type InitError = ConnectServiceError;

    async fn create(&self, cfg: SharedCfg) -> Result<Self::Service, Self::InitError> {
        Ok(ConnectorService::with(cfg))
    }
}

impl<T: Address> Service<Connect<T>> for ConnectorService<T> {
    type Response = Io;
    type Error = ConnectError;

    async fn call(
        &self,
        req: Connect<T>,
        _: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        self.connect(req).await
    }
}

/// Tcp stream connector
async fn connect<T: Address>(
    req: T,
    port: u16,
    addr: Either<SocketAddr, VecDeque<SocketAddr>>,
    cfg: SharedCfg,
) -> Result<Io, ConnectError> {
    log::trace!(
        "{}: TCP connector - connecting to {:?} addr:{addr:?} port:{port}",
        cfg.tag(),
        req.host(),
    );

    let io = match addr {
        Either::Left(addr) => tcp_connect(addr, cfg).await?,
        Either::Right(mut addrs) => loop {
            let addr = addrs.pop_front().unwrap();

            match tcp_connect(addr, cfg).await {
                Ok(io) => break io,
                Err(err) => {
                    log::trace!(
                        "{}: TCP connector - failed to connect to {:?} port: {port} err: {err:?}",
                        cfg.tag(),
                        req.host(),
                    );
                    if addrs.is_empty() {
                        return Err(err.into());
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

#[cfg(test)]
mod tests {
    use super::*;

    use ntex_util::time::Millis;

    #[ntex::test]
    async fn test_connect() {
        let server = ntex::server::test_server(async || {
            ntex_service::fn_service(|_| async { Ok::<_, ()>(()) })
        });

        let srv = Connector::default()
            .create(
                SharedCfg::new("T")
                    .add(IoConfig::new().set_connect_timeout(Millis(5000)))
                    .into(),
            )
            .await
            .unwrap();
        let result = srv.connect("").await;
        assert!(result.is_err());
        let result = srv.connect("localhost:99999").await;
        assert!(result.is_err());
        assert!(format!("{srv:?}").contains("Connector"));

        let srv = ConnectorService::default();
        let result = srv.connect(format!("{}", server.addr())).await;
        assert!(result.is_ok());

        let msg = Connect::new(format!("{}", server.addr())).set_addrs(vec![
            format!("127.0.0.1:{}", server.addr().port() - 1)
                .parse()
                .unwrap(),
            server.addr(),
        ]);
        let result = crate::connect::connect(msg).await;
        assert!(result.is_ok());

        let msg = Connect::new(server.addr());
        let result = crate::connect::connect(msg).await;
        assert!(result.is_ok());
    }
}
