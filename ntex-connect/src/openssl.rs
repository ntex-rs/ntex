use std::io;

pub use ntex_tls::openssl::SslFilter;
pub use tls_openssl::ssl::{Error as SslError, HandshakeError, SslConnector, SslMethod};

use ntex_bytes::PoolId;
use ntex_io::{FilterFactory, Io, Layer};
use ntex_service::{Service, ServiceFactory};
use ntex_tls::openssl::SslConnector as IoSslConnector;
use ntex_util::future::{BoxFuture, Ready};

use super::{Address, Connect, ConnectError, Connector as BaseConnector};

pub struct Connector<T> {
    connector: BaseConnector<T>,
    openssl: SslConnector,
}

impl<T> Connector<T> {
    /// Construct new OpensslConnectService factory
    pub fn new(connector: SslConnector) -> Self {
        Connector {
            connector: BaseConnector::default(),
            openssl: connector,
        }
    }

    /// Set memory pool.
    ///
    /// Use specified memory pool for memory allocations. By default P0
    /// memory pool is used.
    pub fn memory_pool(self, id: PoolId) -> Self {
        Self {
            connector: self.connector.memory_pool(id),
            openssl: self.openssl,
        }
    }
}

impl<T: Address> Connector<T> {
    /// Resolve and connect to remote host
    pub async fn connect<U>(&self, message: U) -> Result<Io<Layer<SslFilter>>, ConnectError>
    where
        Connect<T>: From<U>,
    {
        let message = Connect::from(message);
        let host = message.host().split(':').next().unwrap().to_string();
        let conn = self.connector.call(message);
        let openssl = self.openssl.clone();

        let io = conn.await?;
        trace!("SSL Handshake start for: {:?}", host);

        match openssl.configure() {
            Err(e) => Err(io::Error::new(io::ErrorKind::Other, e).into()),
            Ok(config) => {
                let ssl = config
                    .into_ssl(&host)
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
                match IoSslConnector::new(ssl).create(io).await {
                    Ok(io) => {
                        trace!("SSL Handshake success: {:?}", host);
                        Ok(io)
                    }
                    Err(e) => {
                        trace!("SSL Handshake error: {:?}", e);
                        Err(io::Error::new(io::ErrorKind::Other, format!("{}", e)).into())
                    }
                }
            }
        }
    }
}

impl<T> Clone for Connector<T> {
    fn clone(&self) -> Self {
        Connector {
            connector: self.connector.clone(),
            openssl: self.openssl.clone(),
        }
    }
}

impl<T: Address, C: 'static> ServiceFactory<Connect<T>, C> for Connector<T> {
    type Response = Io<Layer<SslFilter>>;
    type Error = ConnectError;
    type Service = Connector<T>;
    type InitError = ();
    type Future<'f> = Ready<Self::Service, Self::InitError> where Self: 'f;

    #[inline]
    fn create(&self, _: C) -> Self::Future<'_> {
        Ready::Ok(self.clone())
    }
}

impl<T: Address> Service<Connect<T>> for Connector<T> {
    type Response = Io<Layer<SslFilter>>;
    type Error = ConnectError;
    type Future<'f> = BoxFuture<'f, Result<Self::Response, Self::Error>>;

    #[inline]
    fn call(&self, req: Connect<T>) -> Self::Future<'_> {
        Box::pin(self.connect(req))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ntex_service::{Service, ServiceFactory};

    #[ntex::test]
    async fn test_openssl_connect() {
        let server = ntex::server::test_server(|| {
            ntex::service::fn_service(|_| async { Ok::<_, ()>(()) })
        });

        let ssl = SslConnector::builder(SslMethod::tls()).unwrap();
        let factory = Connector::new(ssl.build()).clone().memory_pool(PoolId::P5);

        let srv = factory.create(&()).await.unwrap();
        let result = srv
            .call(Connect::new("").set_addr(Some(server.addr())))
            .await;
        assert!(result.is_err());
    }
}
