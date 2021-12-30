use std::{convert::TryFrom, future::Future, io, pin::Pin, task::Context, task::Poll};

pub use ntex_tls::rustls::TlsFilter;
pub use tls_rustls::{ClientConfig, ServerName};

use ntex_tls::rustls::TlsConnector;

use crate::io::{utils::Boxed, Base, Io};
use crate::service::{Service, ServiceFactory};
use crate::util::{PoolId, Ready};

use super::{Address, Connect, ConnectError, Connector as BaseConnector};

/// Rustls connector factory
pub struct Connector<T> {
    connector: BaseConnector<T>,
    inner: TlsConnector,
}

impl<T> From<std::sync::Arc<ClientConfig>> for Connector<T> {
    fn from(cfg: std::sync::Arc<ClientConfig>) -> Self {
        Connector {
            inner: TlsConnector::new(cfg),
            connector: BaseConnector::default(),
        }
    }
}

impl<T> Connector<T> {
    pub fn new(config: ClientConfig) -> Self {
        Connector {
            inner: TlsConnector::new(std::sync::Arc::new(config)),
            connector: BaseConnector::default(),
        }
    }

    /// Set memory pool.
    ///
    /// Use specified memory pool for memory allocations. By default P0
    /// memory pool is used.
    pub fn memory_pool(self, id: PoolId) -> Self {
        Self {
            connector: self.connector.memory_pool(id),
            inner: self.inner,
        }
    }
}

impl<T: Address + 'static> Connector<T> {
    /// Resolve and connect to remote host
    pub fn connect<U>(
        &self,
        message: U,
    ) -> impl Future<Output = Result<Io<TlsFilter<Base>>, ConnectError>>
    where
        Connect<T>: From<U>,
    {
        let req = Connect::from(message);
        let host = req.host().split(':').next().unwrap().to_owned();
        let conn = self.connector.call(req);
        let connector = self.inner.clone();

        async move {
            let io = conn.await?;
            trace!("SSL Handshake start for: {:?}", host);

            let host = ServerName::try_from(host.as_str())
                .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("{}", e)))?;
            let connector = connector.server_name(host.clone());

            match io.add_filter(connector).await {
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

    /// Produce sealed io stream (IoBoxed)
    pub fn seal(self) -> Boxed<Connector<T>, Connect<T>> {
        Boxed::new(self)
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

impl<T: Address, C> ServiceFactory<Connect<T>, C> for Connector<T> {
    type Response = Io<TlsFilter<Base>>;
    type Error = ConnectError;
    type Service = Connector<T>;
    type InitError = ();
    type Future = Ready<Self::Service, Self::InitError>;

    #[inline]
    fn new_service(&self, _: C) -> Self::Future {
        Ready::Ok(self.clone())
    }
}

impl<T: Address> Service<Connect<T>> for Connector<T> {
    type Response = Io<TlsFilter<Base>>;
    type Error = ConnectError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>>>>;

    #[inline]
    fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&self, req: Connect<T>) -> Self::Future {
        Box::pin(self.connect(req))
    }
}

#[cfg(test)]
mod tests {
    use tls_rustls::{OwnedTrustAnchor, RootCertStore};

    use super::*;
    use crate::service::{Service, ServiceFactory};

    #[crate::rt_test]
    async fn test_rustls_connect() {
        let server = crate::server::test_server(|| {
            crate::service::fn_service(|_| async { Ok::<_, ()>(()) })
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
        let factory = Connector::new(config).clone();

        let srv = factory.new_service(()).await.unwrap();
        let result = srv
            .call(Connect::new("www.rust-lang.org").set_addr(Some(server.addr())))
            .await;
        assert!(result.is_err());
    }
}
