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

impl<'a, T: Address> From<&'a Arc<ClientConfig>> for TlsConnector<T> {
    fn from(config: &'a Arc<ClientConfig>) -> Self {
        TlsConnector {
            config: config.clone(),
            connector: BaseConnector::default().into(),
        }
    }
}

impl<T: Address> TlsConnector<T> {
    pub fn new(config: ClientConfig) -> Self {
        TlsConnector::from(Arc::new(config))
    }

    /// Construct new tls connector
    pub fn with(config: Arc<ClientConfig>, base: BaseConnector<T>) -> Self {
        TlsConnector {
            config,
            connector: base.into(),
        }
    }

    /// Set io tag
    ///
    /// Set tag to opened io object.
    pub fn tag(mut self, tag: &'static str) -> Self {
        self.connector = self.connector.get_ref().tag(tag).into();
        self
    }

    #[deprecated]
    #[doc(hidden)]
    /// Set memory pool.
    ///
    /// Use specified memory pool for memory allocations. By default P0
    /// memory pool is used.
    pub fn memory_pool(self, _: PoolId) -> Self {
        self
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
        let tag = io.tag();

        log::trace!("{tag}: SSL Handshake start for: {host:?}");

        let config = self.config.clone();
        let host = ServerName::try_from(host).map_err(io::Error::other)?;

        match TlsClientFilter::create(io, config, host.clone()).await {
            Ok(io) => {
                log::trace!("{tag}: TLS Handshake success: {host:?}");
                Ok(io)
            }
            Err(e) => {
                log::trace!("{tag}: TLS Handshake error: {e:?}");
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
#[allow(deprecated)]
mod tests {
    use super::*;

    use ntex_util::future::lazy;
    use tls_rust::RootCertStore;

    #[ntex::test]
    async fn test_rustls_connect() {
        let server = ntex::server::test_server(|| {
            ntex::service::fn_service(|_| async {
                io.write("Testing");
                Ok::<_, ()>(()) 
            })
        });

        let cert_store =
            RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let config = ClientConfig::builder()
            .with_root_certificates(cert_store)
            .with_no_client_auth();
        let _ = TlsConnector::<&'static str>::new(config.clone()).clone();
        let factory = TlsConnector::with(Arc::new(config), Default::default())
            .memory_pool(PoolId::P5)
            .tag("IO")
            .clone();

        let srv = factory.pipeline(&()).await.unwrap().bind();
        // always ready
        assert!(lazy(|cx| srv.poll_ready(cx)).await.is_ready());
        let io = srv
            .call(Connect::new("localhost").set_addr(Some(server.addr())))
            .await
            .unwrap();
        let _ = io.read_ready().await;
        assert!(io.is_closed());
    }
}
