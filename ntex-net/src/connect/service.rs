use std::{collections::VecDeque, fmt, io, net::SocketAddr};

use ntex_io::{types, Io, IoConfig};
use ntex_service::{Service, ServiceCtx, ServiceFactory};
use ntex_util::{future::Either, time::timeout_checked, time::Millis};

use super::{Address, Connect, ConnectError, Resolver};
use crate::tcp_connect;

/// Basic tcp stream connector
pub struct Connector<T> {
    cfg: IoConfig,
    timeout: Millis,
    resolver: Resolver<T>,
}

impl<T> Copy for Connector<T> {}

impl<T> Connector<T> {
    /// Construct new connect service with default dns resolver
    pub fn new() -> Self {
        Connector {
            cfg: IoConfig::default(),
            resolver: Resolver::new(),
            timeout: Millis::ZERO,
        }
    }

    /// Set io config
    ///
    /// Set config for new io object.
    pub fn cfg(mut self, cfg: IoConfig) -> Self {
        self.cfg = cfg;
        self
    }

    /// Connect timeout.
    ///
    /// i.e. max time to connect to remote host including dns name resolution.
    /// Timeout is disabled by default
    pub fn timeout<U: Into<Millis>>(mut self, timeout: U) -> Self {
        self.timeout = timeout.into();
        self
    }
}

impl<T: Address> Connector<T> {
    /// Resolve and connect to remote host
    pub async fn connect<U>(&self, message: U) -> Result<Io, ConnectError>
    where
        Connect<T>: From<U>,
    {
        timeout_checked(self.timeout, async {
            // resolve first
            let address = self
                .resolver
                .lookup_with_tag(message.into(), self.cfg.tag())
                .await?;

            let port = address.port();
            let Connect { req, addr, .. } = address;

            if let Some(addr) = addr {
                connect(req, port, addr, self.cfg).await
            } else if let Some(addr) = req.addr() {
                connect(req, addr.port(), Either::Left(addr), self.cfg).await
            } else {
                log::error!("{}: TCP connector: got unresolved address", self.cfg.tag());
                Err(ConnectError::Unresolved)
            }
        })
        .await
        .map_err(|_| ConnectError::Io(io::Error::new(io::ErrorKind::TimedOut, "Timeout")))
        .and_then(|item| item)
    }
}

impl<T> Default for Connector<T> {
    fn default() -> Self {
        Connector::new()
    }
}

impl<T> Clone for Connector<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> fmt::Debug for Connector<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Connector")
            .field("cfg", &self.cfg)
            .field("timeout", &self.timeout)
            .field("resolver", &self.resolver)
            .finish()
    }
}

impl<T: Address, C> ServiceFactory<Connect<T>, C> for Connector<T> {
    type Response = Io;
    type Error = ConnectError;
    type Service = Connector<T>;
    type InitError = ();

    async fn create(&self, _: C) -> Result<Self::Service, Self::InitError> {
        Ok(*self)
    }
}

impl<T: Address> Service<Connect<T>> for Connector<T> {
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
    cfg: IoConfig,
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
                        cfg.tag(), req.host(),
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

    #[ntex::test]
    async fn test_connect() {
        let server = ntex::server::test_server(|| {
            ntex_service::fn_service(|_| async { Ok::<_, ()>(()) })
        });

        let srv = Connector::default()
            .cfg(IoConfig::new("T"))
            .timeout(Millis(5000));
        let result = srv.connect("").await;
        assert!(result.is_err());
        let result = srv.connect("localhost:99999").await;
        assert!(result.is_err());
        assert!(format!("{srv:?}").contains("Connector"));

        let srv = Connector::default();
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
