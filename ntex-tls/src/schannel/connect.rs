use std::io;

use ntex_error::Error;
use ntex_io::{Io, Layer};
use ntex_net::connect::{Address, Connect, ConnectError, Connector, Connector2};
use ntex_service::cfg::{Cfg, SharedCfg};
use ntex_service::{Service, ServiceCtx, ServiceFactory};
use ntex_util::time::timeout_checked;

use super::{SchannelFilter, connect as connect_io};
use crate::TlsConfig;

#[derive(Clone, Debug)]
pub struct TlsConnector<S = Connector<String>> {
    connector: S,
}

#[derive(Clone, Debug)]
pub struct TlsConnectorService<S> {
    svc: S,
    cfg: Cfg<TlsConfig>,
}

impl<A: Address> TlsConnector<Connector<A>> {
    /// Construct new Schannel connector factory.
    pub fn new() -> Self {
        TlsConnector {
            connector: Connector::default(),
        }
    }
}

impl<A: Address, S> ServiceFactory<Connect<A>, SharedCfg> for TlsConnector<S>
where
    S: ServiceFactory<Connect<A>, SharedCfg, Response = Io, Error = ConnectError>,
{
    type Response = Io<Layer<SchannelFilter>>;
    type Error = ConnectError;
    type Service = TlsConnectorService<S::Service>;
    type InitError = S::InitError;

    async fn create(&self, cfg: SharedCfg) -> Result<Self::Service, Self::InitError> {
        let svc = self.connector.create(cfg.clone()).await?;

        Ok(TlsConnectorService {
            svc,
            cfg: cfg.get(),
        })
    }
}

impl<A: Address, S> Service<Connect<A>> for TlsConnectorService<S>
where
    S: Service<Connect<A>, Response = Io, Error = ConnectError>,
{
    type Response = Io<Layer<SchannelFilter>>;
    type Error = ConnectError;

    ntex_service::forward_ready!(svc);
    ntex_service::forward_poll!(svc);
    ntex_service::forward_shutdown!(svc);

    async fn call(
        &self,
        message: Connect<A>,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        let host = message.host().split(':').next().unwrap().to_string();

        let io = ctx.call(&self.svc, message).await?;
        let tag = io.tag();
        log::trace!("{tag}: TLS Handshake start for: {host:?}");

        match timeout_checked(self.cfg.handshake_timeout(), connect_io(io, &host)).await {
            Ok(Ok(io)) => {
                log::trace!("{tag}: TLS Handshake success: {host:?}");
                Ok(io)
            }
            Ok(Err(e)) => {
                log::trace!("{tag}: TLS Handshake error: {e:?}");
                Err(e.into())
            }
            Err(()) => {
                log::trace!("{tag}: TLS Handshake timeout");
                Err(io::Error::new(io::ErrorKind::TimedOut, "TLS Handshake timeout").into())
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct TlsConnector2<S = Connector2<String>> {
    connector: S,
}

#[derive(Clone, Debug)]
pub struct TlsConnectorService2<S> {
    svc: S,
    cfg: Cfg<TlsConfig>,
}

impl<A: Address> TlsConnector2<Connector2<A>> {
    /// Construct new Schannel connector factory.
    pub fn new() -> Self {
        TlsConnector2 {
            connector: Connector2::default(),
        }
    }
}

impl<A: Address, S> ServiceFactory<Connect<A>, SharedCfg> for TlsConnector2<S>
where
    S: ServiceFactory<Connect<A>, SharedCfg, Response = Io, Error = Error<ConnectError>>,
{
    type Response = Io<Layer<SchannelFilter>>;
    type Error = Error<ConnectError>;
    type Service = TlsConnectorService2<S::Service>;
    type InitError = S::InitError;

    async fn create(&self, cfg: SharedCfg) -> Result<Self::Service, Self::InitError> {
        let svc = self.connector.create(cfg.clone()).await?;

        Ok(TlsConnectorService2 {
            svc,
            cfg: cfg.get(),
        })
    }
}

impl<A: Address, S> Service<Connect<A>> for TlsConnectorService2<S>
where
    S: Service<Connect<A>, Response = Io, Error = Error<ConnectError>>,
{
    type Response = Io<Layer<SchannelFilter>>;
    type Error = Error<ConnectError>;

    ntex_service::forward_ready!(svc);
    ntex_service::forward_poll!(svc);
    ntex_service::forward_shutdown!(svc);

    async fn call(
        &self,
        message: Connect<A>,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        let host = message.host().split(':').next().unwrap().to_string();

        let io = ctx.call(&self.svc, message).await?;
        let tag = io.tag();
        log::trace!("{tag}: TLS Handshake start for: {host:?} {io:?}");

        async {
            match timeout_checked(self.cfg.handshake_timeout(), connect_io(io, &host)).await
            {
                Ok(Ok(io)) => {
                    log::trace!("{tag}: TLS Handshake success: {host:?}");
                    Ok(io)
                }
                Ok(Err(e)) => {
                    log::trace!("{tag}: TLS Handshake error: {e:?}");
                    Err(ConnectError::from(e).into())
                }
                Err(()) => {
                    log::trace!("{tag}: TLS Handshake timeout");
                    Err(ConnectError::from(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "TLS Handshake timeout",
                    ))
                    .into())
                }
            }
        }
        .await
        .map_err(|e: Error<_>| e.set_service(self.cfg.service()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[ntex::test]
    async fn test_schannel_connect() {
        let server = ntex::server::test_server(async || {
            ntex::service::fn_service(|_| async { Ok::<_, ()>(()) })
        });

        let _: TlsConnector<Connector<&'static str>> = TlsConnector::new();
        let factory: TlsConnector<Connector<&'static str>> = TlsConnector::new();
        let srv = factory.pipeline(SharedCfg::default()).await.unwrap();
        assert!(srv.ready().await.is_ok());
        let result = srv
            .call(Connect::new("").set_addr(Some(server.addr())))
            .await;
        assert!(result.is_err());
        assert!(format!("{srv:?}").contains("TlsConnector"));
    }

    #[ntex::test]
    async fn test_schannel_connect2() {
        let server = ntex::server::test_server(async || {
            ntex::service::fn_service(|_| async { Ok::<_, ()>(()) })
        });

        let _: TlsConnector2<Connector2<&'static str>> = TlsConnector2::new();
        let factory: TlsConnector2<Connector2<&'static str>> = TlsConnector2::new();
        let srv = factory.pipeline(SharedCfg::default()).await.unwrap();
        assert!(srv.ready().await.is_ok());
        let result = srv
            .call(Connect::new("").set_addr(Some(server.addr())))
            .await;
        assert!(result.is_err());
        assert!(format!("{srv:?}").contains("TlsConnector"));
    }
}
