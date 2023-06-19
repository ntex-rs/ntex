use std::io;

pub use ntex_tls::rustls::TlsFilter;
pub use tls_rustls::{ClientConfig, ServerName};

use ntex_bytes::PoolId;
use ntex_io::{FilterFactory, Io, Layer};
use ntex_service::{Container, Service, ServiceCtx, ServiceFactory};
use ntex_tls::rustls::TlsConnector;
use ntex_util::future::{BoxFuture, Ready};

use super::{Address, Connect, ConnectError, Connector as BaseConnector};

/// Rustls connector factory
pub struct Connector<T> {
    connector: Container<BaseConnector<T>>,
    inner: TlsConnector,
}

impl<T: Address> From<std::sync::Arc<ClientConfig>> for Connector<T> {
    fn from(cfg: std::sync::Arc<ClientConfig>) -> Self {
        Connector {
            inner: TlsConnector::new(cfg),
            connector: BaseConnector::default().into(),
        }
    }
}

impl<T: Address> Connector<T> {
    pub fn new(config: ClientConfig) -> Self {
        Connector {
            inner: TlsConnector::new(std::sync::Arc::new(config)),
            connector: BaseConnector::default().into(),
        }
    }

    /// Set memory pool.
    ///
    /// Use specified memory pool for memory allocations. By default P0
    /// memory pool is used.
    pub fn memory_pool(self, id: PoolId) -> Self {
        let connector = self
            .connector
            .into_service()
            .unwrap()
            .memory_pool(id)
            .into();
        Self {
            connector,
            inner: self.inner,
        }
    }
}

impl<T: Address + 'static> Connector<T> {
    /// Resolve and connect to remote host
    pub async fn connect<U>(&self, message: U) -> Result<Io<Layer<TlsFilter>>, ConnectError>
    where
        Connect<T>: From<U>,
    {
        let req = Connect::from(message);
        let host = req.host().split(':').next().unwrap().to_owned();
        let conn = self.connector.call(req);
        let connector = self.inner.clone();

        let io = conn.await?;
        trace!("SSL Handshake start for: {:?}", host);

        let host = ServerName::try_from(host.as_str())
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("{}", e)))?;
        let connector = connector.server_name(host.clone());

        match connector.create(io).await {
            Ok(io) => {
                trace!("TLS Handshake success: {:?}", &host);
                Ok(io)
            }
            Err(e) => {
                trace!("TLS Handshake error: {:?}", e);
                Err(io::Error::new(io::ErrorKind::Other, format!("{}", e)).into())
            }
        }
    }
}

impl<T> Clone for Connector<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            connector: self.connector.clone(),
        }
    }
}

impl<T: Address, C: 'static> ServiceFactory<Connect<T>, C> for Connector<T> {
    type Response = Io<Layer<TlsFilter>>;
    type Error = ConnectError;
    type Service = Connector<T>;
    type InitError = ();
    type Future<'f> = Ready<Self::Service, Self::InitError> where C: 'f;

    #[inline]
    fn create(&self, _: C) -> Self::Future<'_> {
        Ready::Ok(self.clone())
    }
}

impl<T: Address> Service<Connect<T>> for Connector<T> {
    type Response = Io<Layer<TlsFilter>>;
    type Error = ConnectError;
    type Future<'f> = BoxFuture<'f, Result<Self::Response, Self::Error>>;

    fn call<'a>(&'a self, req: Connect<T>, _: ServiceCtx<'a, Self>) -> Self::Future<'a> {
        Box::pin(self.connect(req))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use tls_rustls::{OwnedTrustAnchor, RootCertStore};

    use super::*;
    use ntex_service::ServiceFactory;
    use ntex_util::future::lazy;

    #[ntex::test]
    async fn test_rustls_connect() {
        let server = ntex::server::test_server(|| {
            ntex::service::fn_service(|_| async { Ok::<_, ()>(()) })
        });

        let mut cert_store = RootCertStore::empty();
        cert_store.add_server_trust_anchors(webpki_roots::TLS_SERVER_ROOTS.0.iter().map(
            |ta| {
                OwnedTrustAnchor::from_subject_spki_name_constraints(
                    ta.subject,
                    ta.spki,
                    ta.name_constraints,
                )
            },
        ));
        let config = ClientConfig::builder()
            .with_safe_defaults()
            .with_root_certificates(cert_store)
            .with_no_client_auth();
        let _ = Connector::<&'static str>::new(config.clone()).clone();
        let factory = Connector::from(Arc::new(config))
            .memory_pool(PoolId::P5)
            .clone();

        let srv = factory.container(&()).await.unwrap();
        // always ready
        assert!(lazy(|cx| srv.poll_ready(cx)).await.is_ready());
        let result = srv
            .call(Connect::new("www.rust-lang.org").set_addr(Some(server.addr())))
            .await;
        assert!(result.is_err());
    }
}
