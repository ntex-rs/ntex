use std::{fmt, io, sync::Arc};

use ntex_io::{Io, Layer};
use ntex_net::connect::{Address, Connect, ConnectError, Connector};
use ntex_service::cfg::{Cfg, SharedCfg};
use ntex_service::{Service, ServiceCtx, ServiceFactory};
use ntex_util::time::timeout_checked;
use tls_rustls::{ClientConfig, pki_types::ServerName};

use crate::{TlsConfig, rustls::TlsClientFilter};

/// Rustls connector factory
pub struct TlsConnector<S> {
    connector: S,
    config: Arc<ClientConfig>,
}

#[derive(Clone, Debug)]
pub struct TlsConnectorService<S> {
    svc: S,
    cfg: Cfg<TlsConfig>,
    config: Arc<ClientConfig>,
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

impl<S: Clone> Clone for TlsConnector<S> {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            connector: self.connector.clone(),
        }
    }
}

impl<S: fmt::Debug> fmt::Debug for TlsConnector<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TlsConnector(rustls)")
            .field("connector", &self.connector)
            .finish()
    }
}

impl<A, S> ServiceFactory<Connect<A>, SharedCfg> for TlsConnector<S>
where
    A: Address,
    S: ServiceFactory<Connect<A>, SharedCfg, Response = Io, Error = ConnectError>,
{
    type Response = Io<Layer<TlsClientFilter>>;
    type Error = ConnectError;
    type Service = TlsConnectorService<S::Service>;
    type InitError = S::InitError;

    async fn create(&self, cfg: SharedCfg) -> Result<Self::Service, Self::InitError> {
        let svc = self.connector.create(cfg.clone()).await?;

        Ok(TlsConnectorService {
            svc,
            cfg: cfg.get(),
            config: self.config.clone(),
        })
    }
}

impl<A: Address, S> Service<Connect<A>> for TlsConnectorService<S>
where
    S: Service<Connect<A>, Response = Io, Error = ConnectError>,
{
    type Response = Io<Layer<TlsClientFilter>>;
    type Error = ConnectError;

    ntex_service::forward_ready!(svc);
    ntex_service::forward_poll!(svc);
    ntex_service::forward_shutdown!(svc);

    async fn call(
        &self,
        req: Connect<A>,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        let host = req.host().split(':').next().unwrap().to_owned();

        let io = ctx.call(&self.svc, req).await?;
        let tag = io.tag();
        log::trace!("{tag}: TLS Handshake start for: {host:?}");

        let config = self.config.clone();
        let host = ServerName::try_from(host).map_err(io::Error::other)?;

        let connect_fut = TlsClientFilter::create(io, config, host.clone());
        match timeout_checked(self.cfg.handshake_timeout(), connect_fut).await {
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
                Err(io::Error::new(io::ErrorKind::TimedOut, "SSL Handshake timeout").into())
            }
        }
    }
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

        let srv = factory.pipeline(SharedCfg::default()).await.unwrap().bind();
        // always ready
        assert!(lazy(|cx| srv.poll_ready(cx)).await.is_ready());
        let result = srv
            .call(Connect::new("").set_addr(Some(server.addr())))
            .await;
        assert!(result.is_err());
        assert!(format!("{srv:?}").contains("TlsConnector"));
    }
}
