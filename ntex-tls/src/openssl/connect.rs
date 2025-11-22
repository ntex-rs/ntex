use std::{fmt, io};

use ntex_io::{Io, IoConfig, Layer};
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

    /// Construct new openssl connector
    pub fn with(config: BaseSslConnector, base: BaseConnector<T>) -> Self {
        SslConnector {
            openssl: config,
            connector: base.into(),
        }
    }

    /// Set io configuration
    pub fn cfg(mut self, cfg: IoConfig) -> Self {
        self.connector = self.connector.get_ref().cfg(cfg).into();
        self
    }

    #[deprecated]
    #[doc(hidden)]
    /// Set io tag
    pub fn tag(self, _: &'static str) -> Self {
        self
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
        let tag = io.tag();
        log::trace!("{tag}: SSL Handshake start for: {host:?}");

        match openssl.configure() {
            Err(e) => Err(io::Error::new(io::ErrorKind::InvalidInput, e).into()),
            Ok(config) => {
                let ssl = config
                    .into_ssl(&host)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
                match connect_io(io, ssl).await {
                    Ok(io) => {
                        log::trace!("{tag}: SSL Handshake success: {host:?}");
                        Ok(io)
                    }
                    Err(e) => {
                        log::trace!("{tag}: SSL Handshake error: {e:?}");
                        Err(io::Error::new(io::ErrorKind::InvalidInput, format!("{e}"))
                            .into())
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
#[allow(deprecated)]
mod tests {
    use super::*;

    use tls_openssl::ssl::SslMethod;

    #[ntex::test]
    async fn test_openssl_connect() {
        let server = ntex::server::test_server(|| {
            ntex::service::fn_service(|_| async { Ok::<_, ()>(()) })
        });

        let ssl = BaseSslConnector::builder(SslMethod::tls()).unwrap();
        let _ = SslConnector::<&'static str>::new(ssl.build());
        let ssl = BaseSslConnector::builder(SslMethod::tls()).unwrap();
        let factory = SslConnector::with(ssl.build(), Default::default())
            .cfg(ntex_io::IoConfig::new("IO"))
            .clone();

        let srv = factory.pipeline(&()).await.unwrap();
        // always ready
        assert!(srv.ready().await.is_ok());
        let result = srv
            .call(Connect::new("").set_addr(Some(server.addr())))
            .await;
        assert!(result.is_err());
        assert!(format!("{srv:?}").contains("SslConnector"));
    }
}
