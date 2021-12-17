use std::{future::Future, io, pin::Pin, task::Context, task::Poll};

use ntex_tls::openssl::{SslConnector as IoSslConnector, SslFilter};
pub use tls_openssl::ssl::{Error as SslError, HandshakeError, SslConnector, SslMethod};

use crate::io::{DefaultFilter, Io};
use crate::service::{Service, ServiceFactory};
use crate::util::{PoolId, Ready};

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

impl<T: Address + 'static> Connector<T> {
    /// Resolve and connect to remote host
    pub fn connect<U>(
        &self,
        message: U,
    ) -> impl Future<Output = Result<Io<SslFilter<DefaultFilter>>, ConnectError>>
    where
        Connect<T>: From<U>,
    {
        let message = Connect::from(message);
        let host = message.host().to_string();
        let conn = self.connector.call(message);
        let openssl = self.openssl.clone();

        async move {
            let io = conn.await?;
            trace!("SSL Handshake start for: {:?}", host);

            match openssl.configure() {
                Err(e) => Err(io::Error::new(io::ErrorKind::Other, e).into()),
                Ok(config) => {
                    let ssl = config
                        .into_ssl(&host)
                        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
                    match io.add_filter(IoSslConnector::new(ssl)).await {
                        Ok(io) => {
                            trace!("SSL Handshake success: {:?}", host);
                            Ok(io)
                        }
                        Err(e) => {
                            trace!("SSL Handshake error: {:?}", e);
                            Err(io::Error::new(io::ErrorKind::Other, format!("{}", e))
                                .into())
                        }
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

impl<T: Address + 'static> ServiceFactory for Connector<T> {
    type Request = Connect<T>;
    type Response = Io<SslFilter<DefaultFilter>>;
    type Error = ConnectError;
    type Config = ();
    type Service = Connector<T>;
    type InitError = ();
    type Future = Ready<Self::Service, Self::InitError>;

    fn new_service(&self, _: ()) -> Self::Future {
        Ready::Ok(self.clone())
    }
}

impl<T: Address + 'static> Service for Connector<T> {
    type Request = Connect<T>;
    type Response = Io<SslFilter<DefaultFilter>>;
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
    use super::*;
    use crate::service::{Service, ServiceFactory};

    #[crate::rt_test]
    async fn test_openssl_connect() {
        let server = crate::server::test_server(|| {
            crate::service::fn_service(|_| async { Ok::<_, ()>(()) })
        });

        let ssl = SslConnector::builder(SslMethod::tls()).unwrap();
        let factory = Connector::new(ssl.build()).clone();

        let srv = factory.new_service(()).await.unwrap();
        let result = srv
            .call(Connect::new("").set_addr(Some(server.addr())))
            .await;
        assert!(result.is_err());
    }
}
