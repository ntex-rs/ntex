use std::{fmt, io, sync::Arc};

use ntex_bytes::PoolId;
use ntex_io::{Io, Layer};
use ntex_net::connect::{Address, Connect, ConnectError, Connector as BaseConnector};
use ntex_service::{Pipeline, Service, ServiceCtx, ServiceFactory};
use tls_rust::{pki_types::ServerName, ClientConfig};

use super::TlsClientFilter;

/// Rustls connector factory
pub struct TlsConnector<T> {
    connector: Pipeline<BaseConnector<T>>,
    config: Arc<ClientConfig>,
}

impl<T: Address> From<Arc<ClientConfig>> for TlsConnector<T> {
    fn from(config: Arc<ClientConfig>) -> Self {
        TlsConnector {
            config,
            connector: BaseConnector::default().into(),
        }
    }
}

impl<T: Address> TlsConnector<T> {
    pub fn new(config: ClientConfig) -> Self {
        TlsConnector {
            config: Arc::new(config),
            connector: BaseConnector::default().into(),
        }
    }

    /// Set memory pool.
    ///
    /// Use specified memory pool for memory allocations. By default P0
    /// memory pool is used.
    pub fn memory_pool(self, id: PoolId) -> Self {
        let connector = self.connector.get_ref().memory_pool(id).into();
        Self {
            connector,
            config: self.config,
        }
    }
}

impl<T: Address> TlsConnector<T> {
    /// Resolve and connect to remote host
    pub async fn connect<U>(
        &self,
        message: U,
    ) -> Result<Io<Layer<TlsClientFilter>>, ConnectError>
    where
        Connect<T>: From<U>,
    {
        let req = Connect::from(message);
        let host = req.host().split(':').next().unwrap().to_owned();
        let io = self.connector.call(req).await?;

        log::trace!("{}: SSL Handshake start for: {:?}", io.tag(), host);

        let tag = io.tag();
        let config = self.config.clone();
        let host = ServerName::try_from(host)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("{}", e)))?;

        match TlsClientFilter::create(io, config, host.clone()).await {
            Ok(io) => {
                log::trace!("{}: TLS Handshake success: {:?}", tag, &host);
                Ok(io)
            }
            Err(e) => {
                log::trace!("{}: TLS Handshake error: {:?}", tag, e);
                Err(e.into())
            }
        }
    }
}

impl<T> Clone for TlsConnector<T> {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            connector: self.connector.clone(),
        }
    }
}

impl<T> fmt::Debug for TlsConnector<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TlsConnector(rustls)")
            .field("connector", &self.connector)
            .finish()
    }
}

impl<T: Address, C> ServiceFactory<Connect<T>, C> for TlsConnector<T> {
    type Response = Io<Layer<TlsClientFilter>>;
    type Error = ConnectError;
    type Service = TlsConnector<T>;
    type InitError = ();

    async fn create(&self, _: C) -> Result<Self::Service, Self::InitError> {
        Ok(self.clone())
    }
}

impl<T: Address> Service<Connect<T>> for TlsConnector<T> {
    type Response = Io<Layer<TlsClientFilter>>;
    type Error = ConnectError;

    async fn call(
        &self,
        req: Connect<T>,
        _: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        self.connect(req).await
    }
}

#[cfg(test)]
mod tests {
    use tls_rust::RootCertStore;

    use super::*;
    use ntex_util::future::lazy;

    #[ntex::test]
    async fn test_rustls_connect() {
        let server = ntex::server::test_server(|| {
            ntex::service::fn_service(|_| async { Ok::<_, ()>(()) })
        });

        let cert_store =
            RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let config = ClientConfig::builder()
            .with_root_certificates(cert_store)
            .with_no_client_auth();
        let _ = TlsConnector::<&'static str>::new(config.clone()).clone();
        let factory = TlsConnector::from(Arc::new(config))
            .memory_pool(PoolId::P5)
            .clone();

        let srv = factory.pipeline(&()).await.unwrap().bind();
        // always ready
        assert!(lazy(|cx| srv.poll_ready(cx)).await.is_ready());
        let result = srv
            .call(Connect::new("www.rust-lang.org").set_addr(Some(server.addr())))
            .await;
        assert!(result.is_err());
    }
}
