use std::{future::Future, io, pin::Pin, sync::Arc, task::Context, task::Poll};

pub use rust_tls::Session;
pub use tokio_rustls::{client::TlsStream, rustls::ClientConfig};

use tokio_rustls::{self, TlsConnector};
use webpki::DNSNameRef;

use crate::rt::net::TcpStream;
use crate::service::{Service, ServiceFactory};
use crate::util::Ready;

use super::{Address, Connect, ConnectError, Connector};

/// Rustls connector factory
pub struct RustlsConnector<T> {
    connector: Connector<T>,
    config: Arc<ClientConfig>,
}

impl<T> RustlsConnector<T> {
    pub fn new(config: Arc<ClientConfig>) -> Self {
        RustlsConnector {
            config,
            connector: Connector::default(),
        }
    }
}

impl<T: Address + 'static> RustlsConnector<T> {
    /// Resolve and connect to remote host
    pub fn connect<U>(
        &self,
        message: U,
    ) -> impl Future<Output = Result<TlsStream<TcpStream>, ConnectError>>
    where
        Connect<T>: From<U>,
    {
        let req = Connect::from(message);
        let host = req.host().to_string();
        let conn = self.connector.call(req);
        let config = self.config.clone();

        async move {
            let io = conn.await?;
            trace!("SSL Handshake start for: {:?}", host);

            let host = DNSNameRef::try_from_ascii_str(&host)
                .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("{}", e)))?;

            match TlsConnector::from(config).connect(host, io).await {
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

impl<T> Clone for RustlsConnector<T> {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            connector: self.connector.clone(),
        }
    }
}

impl<T: Address + 'static> ServiceFactory for RustlsConnector<T> {
    type Request = Connect<T>;
    type Response = TlsStream<TcpStream>;
    type Error = ConnectError;
    type Config = ();
    type Service = RustlsConnector<T>;
    type InitError = ();
    type Future = Ready<Self::Service, Self::InitError>;

    fn new_service(&self, _: ()) -> Self::Future {
        Ready::Ok(self.clone())
    }
}

impl<T: Address + 'static> Service for RustlsConnector<T> {
    type Request = Connect<T>;
    type Response = TlsStream<TcpStream>;
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
    async fn test_rustls_connect() {
        let server = crate::server::test_server(|| {
            crate::fn_service(|_| async { Ok::<_, ()>(()) })
        });

        let mut config = ClientConfig::new();
        config
            .root_store
            .add_server_trust_anchors(&webpki_roots::TLS_SERVER_ROOTS);
        let factory = RustlsConnector::new(Arc::new(config)).clone();

        let srv = factory.new_service(()).await.unwrap();
        let result = srv
            .call(Connect::new("www.rust-lang.org").set_addr(Some(server.addr())))
            .await;
        assert!(result.is_err());
    }
}
