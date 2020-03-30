use std::io;
use std::task::{Context, Poll};

pub use open_ssl::ssl::{Error as SslError, SslConnector, SslMethod};
pub use tokio_openssl::{HandshakeError, SslStream};

use futures::future::{ok, FutureExt, LocalBoxFuture, Ready};
use trust_dns_resolver::AsyncResolver;

use crate::connect::{Address, Connect, ConnectError, Connector};
use crate::rt::net::TcpStream;
use crate::service::{Service, ServiceFactory};

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

    /// Construct new connect service with custom dns resolver
    pub fn with_resolver(connector: SslConnector, resolver: AsyncResolver) -> Self {
        OpensslConnector {
            connector: Connector::new(resolver),
            openssl: connector,
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
    type Future = Ready<Result<Self::Service, Self::InitError>>;

    fn new_service(&self, _: ()) -> Self::Future {
        ok(self.clone())
    }
}

impl<T: Address + 'static> Service for OpensslConnector<T> {
    type Request = Connect<T>;
    type Response = SslStream<TcpStream>;
    type Error = ConnectError;
    type Future = LocalBoxFuture<'static, Result<Self::Response, Self::Error>>;

    #[inline]
    fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&self, req: Connect<T>) -> Self::Future {
        let host = req.host().to_string();
        let conn = self.connector.call(req);
        let openssl = self.openssl.clone();

        async move {
            let io = conn.await?;
            trace!("SSL Handshake start for: {:?}", host);

            match openssl.configure() {
                Err(e) => Err(io::Error::new(io::ErrorKind::Other, e).into()),
                Ok(config) => match tokio_openssl::connect(config, &host, io).await {
                    Ok(io) => {
                        trace!("SSL Handshake success: {:?}", host);
                        Ok(io)
                    }
                    Err(e) => {
                        trace!("SSL Handshake error: {:?}", e);
                        Err(io::Error::new(io::ErrorKind::Other, format!("{}", e))
                            .into())
                    }
                },
            }
        }
        .boxed_local()
    }
}
