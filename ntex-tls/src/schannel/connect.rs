use std::{io, marker::PhantomData};

use ntex_error::Error;
use ntex_io::{Io, Layer};
use ntex_net::connect::{Address, Connect, ConnectError, Connector};
use ntex_service::cfg::{Cfg, SharedCfg};
use ntex_service::{Ctx, Service, ServiceFactory};
use ntex_util::time::timeout_checked;

use super::{ClientConfig, SchannelFilter, connect as connect_io};
use crate::TlsConfig;

#[derive(Clone, Debug)]
pub struct TlsConnector<Sf> {
    connector: Sf,
    config: ClientConfig,
}

#[derive(Clone, Debug)]
pub struct TlsConnectorService<S, St> {
    svc: S,
    cfg: Cfg<TlsConfig>,
    config: ClientConfig,
    st: PhantomData<St>,
}

impl<A: Address> Default for TlsConnector<Connector<A>> {
    fn default() -> Self {
        Self::new()
    }
}

impl<A: Address> TlsConnector<Connector<A>> {
    /// Construct new Schannel connector factory.
    pub fn new() -> Self {
        TlsConnector {
            connector: Connector::default(),
            config: ClientConfig::default(),
        }
    }

    /// Construct new Schannel connector factory with custom configuration.
    pub fn with_config(config: ClientConfig) -> Self {
        TlsConnector {
            connector: Connector::default(),
            config,
        }
    }
}

impl<A: Address, Sf> ServiceFactory<Connect<A>> for TlsConnector<Sf>
where
    Sf: ServiceFactory<
            Connect<A>,
            Res = Io,
            Error = Error<ConnectError>,
            InitCfg = SharedCfg,
        >,
{
    type St = Sf::St;
    type Res = Io<Layer<SchannelFilter>>;
    type Error = Error<ConnectError>;

    type Service = TlsConnectorService<Sf::Service, Sf::St>;
    type InitCfg = SharedCfg;
    type InitError = Sf::InitError;

    async fn create(&self, cfg: &SharedCfg) -> Result<Self::Service, Self::InitError> {
        let svc = self.connector.create(cfg).await?;

        Ok(TlsConnectorService {
            svc,
            cfg: cfg.get(),
            config: self.config.clone(),
            st: PhantomData,
        })
    }
}

impl<A: Address, S, St> Service for TlsConnectorService<S, St>
where
    S: Service<St = St, Req = Connect<A>, Res = Io, Error = Error<ConnectError>>,
{
    type St = St;
    type Req = Connect<A>;
    type Res = Io<Layer<SchannelFilter>>;
    type Error = Error<ConnectError>;

    ntex_service::forward_ready!(svc);
    ntex_service::forward_poll!(svc);
    ntex_service::forward_shutdown!(svc);

    async fn call(
        &self,
        message: Connect<A>,
        ctx: Ctx<'_, Self>,
    ) -> Result<Self::Res, Self::Error> {
        let host = message.host().split(':').next().unwrap().to_string();

        let io = ctx.call(&self.svc, message).await?;
        let tag = io.tag();
        log::trace!("{tag}: TLS Handshake start for: {host:?}");

        match timeout_checked(
            self.cfg.handshake_timeout(),
            connect_io(io, &host, self.config.clone()),
        )
        .await
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
}
