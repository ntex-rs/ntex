use std::{future::Future, io, pin::Pin, task::Context, task::Poll};

pub use open_ssl::ssl::{Error as SslError, HandshakeError, SslConnector, SslMethod};
pub use tokio_openssl::SslStream;

use crate::rt::net::TcpStream;
use crate::service::{Service, ServiceFactory};
use crate::util::Ready;

use super::{Address, Connect, ConnectError, Connector};

pub struct OpensslConnector<T> {
    connector: Connector<T>,
    openssl: SslConnector,
}

impl<T> OpensslConnector<T> {
    /// Construct new OpensslConnectService factory
    pub fn new(connector: SslConnector) -> Self {
        OpensslConnector {
            connector: Connector::default(),
            openssl: connector,
        }
    }
}

impl<T: Address + 'static> OpensslConnector<T> {
    /// Resolve and connect to remote host
    pub fn connect<U>(
        &self,
        message: U,
    ) -> impl Future<Output = Result<SslStream<TcpStream>, ConnectError>>
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
                    let config = config
                        .into_ssl(&host)
                        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
                    let mut io = SslStream::new(config, io)
                        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
                    match Pin::new(&mut io).connect().await {
                        Ok(_) => {
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

impl<T> Clone for OpensslConnector<T> {
    fn clone(&self) -> Self {
        OpensslConnector {
            connector: self.connector.clone(),
            openssl: self.openssl.clone(),
        }
    }
}

impl<T: Address + 'static> ServiceFactory for OpensslConnector<T> {
    type Request = Connect<T>;
    type Response = SslStream<TcpStream>;
    type Error = ConnectError;
    type Config = ();
    type Service = OpensslConnector<T>;
    type InitError = ();
    type Future = Ready<Self::Service, Self::InitError>;

    fn new_service(&self, _: ()) -> Self::Future {
        Ready::Ok(self.clone())
    }
}

impl<T: Address + 'static> Service for OpensslConnector<T> {
    type Request = Connect<T>;
    type Response = SslStream<TcpStream>;
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
            crate::fn_service(|_| async { Ok::<_, ()>(()) })
        });

        let ssl = SslConnector::builder(SslMethod::tls()).unwrap();
        let factory = OpensslConnector::new(ssl.build()).clone();

        let srv = factory.new_service(()).await.unwrap();
        let result = srv
            .call(Connect::new("").set_addr(Some(server.addr())))
            .await;
        assert!(result.is_err());
    }
}
