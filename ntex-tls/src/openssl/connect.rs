use std::{fmt, io};

use ntex_bytes::PoolId;
use ntex_io::{Io, Layer};
use ntex_net::connect::{Address, Connect, ConnectError, Connector as BaseConnector};
use ntex_service::{Pipeline, Service, ServiceCtx, ServiceFactory};
use tls_openssl::ssl::SslConnector as BaseSslConnector;

use super::{connect as connect_io, SslFilter};

pub struct SslConnector<T> {
    connector: Pipeline<BaseConnector<T>>,
    openssl: BaseSslConnector,
}

impl<T: Address> SslConnector<T> {
    /// Construct new OpensslConnectService factory
    pub fn new(connector: BaseSslConnector) -> Self {
        SslConnector {
            connector: BaseConnector::default().into(),
            openssl: connector,
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
            openssl: self.openssl,
        }
    }
}

impl<T: Address> SslConnector<T> {
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
        log::trace!("{}: SSL Handshake start for: {:?}", io.tag(), host);

        match openssl.configure() {
            Err(e) => Err(io::Error::new(io::ErrorKind::Other, e).into()),
            Ok(config) => {
                let ssl = config
                    .into_ssl(&host)
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
                let tag = io.tag();
                match connect_io(io, ssl).await {
                    Ok(io) => {
                        log::trace!("{}: SSL Handshake success: {:?}", tag, host);
                        Ok(io)
                    }
                    Err(e) => {
                        log::trace!("{}: SSL Handshake error: {:?}", tag, e);
                        Err(io::Error::new(io::ErrorKind::Other, format!("{}", e)).into())
                    }
                }
            }
        }
    }
}

impl<T> Clone for SslConnector<T> {
    fn clone(&self) -> Self {
        Self {
            connector: self.connector.clone(),
            openssl: self.openssl.clone(),
        }
    }
}

impl<T> fmt::Debug for SslConnector<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SslConnector(openssl)")
            .field("connector", &self.connector)
            .field("openssl", &self.openssl)
            .finish()
    }
}

impl<T: Address, C> ServiceFactory<Connect<T>, C> for SslConnector<T> {
    type Response = Io<Layer<SslFilter>>;
    type Error = ConnectError;
    type Service = SslConnector<T>;
    type InitError = ();

    async fn create(&self, _: C) -> Result<Self::Service, Self::InitError> {
        Ok(self.clone())
    }
}

impl<T: Address> Service<Connect<T>> for SslConnector<T> {
    type Response = Io<Layer<SslFilter>>;
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
    use tls_openssl::ssl::SslMethod;

    use super::*;

    #[ntex::test]
    async fn test_openssl_connect() {
        let server = ntex::server::test_server(|| {
            ntex::service::fn_service(|_| async { Ok::<_, ()>(()) })
        });

        let ssl = BaseSslConnector::builder(SslMethod::tls()).unwrap();
        let factory = SslConnector::new(ssl.build())
            .memory_pool(PoolId::P5)
            .clone();

        let srv = factory.pipeline(&()).await.unwrap();
        let result = srv
            .call(Connect::new("").set_addr(Some(server.addr())))
            .await;
        assert!(result.is_err());
        assert!(format!("{:?}", srv).contains("SslConnector"));
    }
}
