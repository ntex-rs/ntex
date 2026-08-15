use std::{fmt, io, marker::PhantomData, sync::Arc};

use ntex_error::Error;
use ntex_io::{Io, Layer};
use ntex_net::connect::{Address, Connect, ConnectError, Connector};
use ntex_service::{Ctx, Service, ServiceFactory, cfg::Cfg, cfg::SharedCfg};
use ntex_util::time::timeout_checked;
use tls_rustls::{ClientConfig, pki_types::ServerName};

use crate::{TlsConfig, rustls::TlsClientFilter};

/// Rustls connector factory
pub struct TlsConnector<Sf> {
    connector: Sf,
    config: Arc<ClientConfig>,
}

#[derive(Clone, Debug)]
pub struct TlsConnectorService<S, St> {
    svc: S,
    cfg: Cfg<TlsConfig>,
    config: Arc<ClientConfig>,
    st: PhantomData<St>,
}

impl<A: Address> From<Arc<ClientConfig>> for TlsConnector<Connector<A>> {
    fn from(config: Arc<ClientConfig>) -> Self {
        TlsConnector {
            config,
            connector: Connector::default(),
        }
    }
}

impl<'a, A: Address> From<&'a Arc<ClientConfig>> for TlsConnector<Connector<A>> {
    fn from(config: &'a Arc<ClientConfig>) -> Self {
        TlsConnector {
            config: config.clone(),
            connector: Connector::default(),
        }
    }
}

impl<A: Address> TlsConnector<Connector<A>> {
    pub fn new(config: ClientConfig) -> Self {
        TlsConnector::from(Arc::new(config))
    }
}

impl<Sf: Clone> Clone for TlsConnector<Sf> {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            connector: self.connector.clone(),
        }
    }
}

impl<Sf: fmt::Debug> fmt::Debug for TlsConnector<Sf> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TlsConnector(rustls)")
            .field("connector", &self.connector)
            .finish()
    }
}

impl<A, Sf> ServiceFactory<Connect<A>> for TlsConnector<Sf>
where
    A: Address,
    Sf: ServiceFactory<
            Connect<A>,
            Res = Io,
            Error = Error<ConnectError>,
            InitCfg = SharedCfg,
        >,
{
    type St = Sf::St;
    type Res = Io<Layer<TlsClientFilter>>;
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
    type Res = Io<Layer<TlsClientFilter>>;
    type Error = Error<ConnectError>;

    async fn call(
        &self,
        req: Connect<A>,
        ctx: Ctx<'_, Self>,
    ) -> Result<Self::Res, Self::Error> {
        let host = req.host().split(':').next().unwrap().to_owned();

        let io = ctx.call(&self.svc, req).await?;
        let tag = io.tag();
        log::trace!("{tag}: TLS Handshake start for: {host:?}");

        let config = self.config.clone();

        async {
            let host = ServerName::try_from(host)
                .map_err(|e| ConnectError::from(io::Error::other(e)))?;

            let connect_fut = TlsClientFilter::create(io, config, host.clone());
            match timeout_checked(self.cfg.handshake_timeout(), connect_fut).await {
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
                        "SSL Handshake timeout",
                    ))
                    .into())
                }
            }
        }
        .await
        .map_err(|e: Error<_>| e.set_service(self.cfg.service()))
    }

    ntex_service::forward_ready!(svc);
    ntex_service::forward_shutdown!(svc);
}

#[cfg(test)]
mod tests {
    use super::*;

    use ntex_util::future::lazy;
    use tls_rustls::RootCertStore;

    #[ntex::test]
    async fn test_rustls_connect() {
        let server = ntex::server::test_server(async || {
            ntex::service::fn_service(|_| async { Ok::<_, ()>(()) })
        });

        let cert_store = webpki_roots::TLS_SERVER_ROOTS
            .iter()
            .cloned()
            .collect::<RootCertStore>();
        let config = ClientConfig::builder()
            .with_root_certificates(cert_store)
            .with_no_client_auth();
        let _: TlsConnector<Connector<&'static str>> =
            TlsConnector::new(config.clone()).clone();
        let factory = TlsConnector::from(Arc::new(config)).clone();
        assert!(
            format!("{factory:?}").contains("TlsConnector"),
            "{factory:?}"
        );

        let srv = factory.pipeline(&SharedCfg::default()).await.unwrap();
        // always ready
        assert!(lazy(|cx| srv.poll_ready(cx)).await.is_ready());
        let result = srv
            .call(Connect::new("").set_addr(Some(server.addr())))
            .await;
        assert!(result.is_err());
        assert!(format!("{srv:?}").contains("TlsConnectorService"));
    }
}
