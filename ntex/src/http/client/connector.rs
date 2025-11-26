use std::{task::Context, time::Duration};

use crate::connect::{Connect as TcpConnect, Connector as TcpConnector};
use crate::io::SharedConfig;
use crate::service::{Service, ServiceCtx, ServiceFactory, apply_fn_factory, boxed};
use crate::{http::Uri, io::IoBoxed, time::Seconds, util::join};

use super::{Connect, Connection, error::ConnectError, pool::ConnectionPool};

#[cfg(feature = "openssl")]
use tls_openssl::ssl::SslConnector as OpensslConnector;

#[cfg(feature = "rustls")]
use tls_rustls::ClientConfig;

type BoxedConnector =
    boxed::BoxServiceFactory<SharedConfig, Connect, IoBoxed, ConnectError, ()>;

#[derive(Debug)]
/// Manages http client network connectivity.
///
/// The `Connector` type uses a builder-like combinator pattern for service
/// construction that finishes by calling the `.finish()` method.
///
/// ```rust,no_run
/// use ntex::http::client::Connector;
///
/// let connector = Connector::default()
///      .keep_alive(5_000);
/// ```
pub struct Connector {
    conn_lifetime: Duration,
    conn_keep_alive: Duration,
    limit: usize,
    connector: BoxedConnector,
    secure_connector: Option<BoxedConnector>,
}

impl Default for Connector {
    fn default() -> Self {
        Connector::new()
    }
}

impl Connector {
    pub fn new() -> Connector {
        let conn = Connector {
            connector: boxed::factory(
                apply_fn_factory(TcpConnector::new(), |msg: Connect, svc| async move {
                    svc.call(TcpConnect::new(msg.uri).set_addr(msg.addr)).await
                })
                .map(IoBoxed::from)
                .map_err(ConnectError::from),
            ),
            secure_connector: None,
            conn_lifetime: Duration::from_secs(75),
            conn_keep_alive: Duration::from_secs(15),
            limit: 100,
        };

        #[cfg(feature = "openssl")]
        {
            use tls_openssl::ssl::SslMethod;

            let mut ssl = OpensslConnector::builder(SslMethod::tls()).unwrap();
            let _ = ssl
                .set_alpn_protos(b"\x02h2\x08http/1.1")
                .map_err(|e| log::error!("Cannot set ALPN protocol: {e:?}"));

            ssl.set_verify(tls_openssl::ssl::SslVerifyMode::NONE);

            conn.openssl(ssl.build())
        }
        #[cfg(all(not(feature = "openssl"), feature = "rustls"))]
        {
            use tls_rustls::RootCertStore;

            let protos = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
            let cert_store =
                RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let mut config = ClientConfig::builder()
                .with_root_certificates(cert_store)
                .with_no_client_auth();
            config.alpn_protocols = protos;
            conn.rustls(config)
        }
        #[cfg(not(any(feature = "openssl", feature = "rustls")))]
        {
            conn
        }
    }
}

impl Connector {
    #[cfg(feature = "openssl")]
    /// Use openssl connector for secured connections.
    pub fn openssl(self, connector: OpensslConnector) -> Self {
        use crate::connect::openssl::SslConnector;

        self.secure_connector(SslConnector::new(connector))
    }

    #[cfg(feature = "rustls")]
    /// Use rustls connector for secured connections.
    pub fn rustls(self, connector: ClientConfig) -> Self {
        use crate::connect::rustls::TlsConnector;

        self.secure_connector(TlsConnector::new(connector))
    }

    /// Set total number of simultaneous connections per type of scheme.
    ///
    /// If limit is 0, the connector has no limit.
    /// The default limit size is 100.
    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    /// Set keep-alive period for opened connection.
    ///
    /// Keep-alive period is the period between connection usage. If
    /// the delay between repeated usages of the same connection
    /// exceeds this period, the connection is closed.
    /// Default keep-alive period is 15 seconds.
    pub fn keep_alive<T: Into<Seconds>>(mut self, dur: T) -> Self {
        self.conn_keep_alive = dur.into().into();
        self
    }

    /// Set max lifetime period for connection.
    ///
    /// Connection lifetime is max lifetime of any opened connection
    /// until it is closed regardless of keep-alive period.
    /// Default lifetime period is 75 seconds.
    pub fn lifetime<T: Into<Seconds>>(mut self, dur: T) -> Self {
        self.conn_lifetime = dur.into().into();
        self
    }

    /// Use custom connector to open un-secured connections.
    pub fn connector<T>(mut self, connector: T) -> Self
    where
        T: ServiceFactory<
                TcpConnect<Uri>,
                SharedConfig,
                Error = crate::connect::ConnectError,
            > + 'static,
        IoBoxed: From<T::Response>,
    {
        self.connector = boxed::factory(
            apply_fn_factory(connector, |msg: Connect, svc| async move {
                svc.call(TcpConnect::new(msg.uri).set_addr(msg.addr)).await
            })
            .map(IoBoxed::from)
            .map_err(ConnectError::from)
            .map_init_err(|_| ()),
        );
        self
    }

    /// Use custom connector to open secure connections.
    pub fn secure_connector<T>(mut self, connector: T) -> Self
    where
        T: ServiceFactory<
                TcpConnect<Uri>,
                SharedConfig,
                Error = crate::connect::ConnectError,
            > + 'static,
        IoBoxed: From<T::Response>,
    {
        self.secure_connector = Some(boxed::factory(
            apply_fn_factory(connector, |msg: Connect, svc| async move {
                svc.call(TcpConnect::new(msg.uri).set_addr(msg.addr)).await
            })
            .map(IoBoxed::from)
            .map_err(ConnectError::from)
            .map_init_err(|_| ()),
        ));
        self
    }
}

impl ServiceFactory<Connect, SharedConfig> for Connector {
    type Response = Connection;
    type Error = ConnectError;
    type Service = ConnectorService;
    type InitError = ();

    async fn create(&self, cfg: SharedConfig) -> Result<Self::Service, Self::InitError> {
        let tcp_pool = ConnectionPool::new(
            self.connector.create(cfg).await?.into(),
            self.conn_lifetime,
            self.conn_keep_alive,
            self.limit,
            cfg,
        );
        let ssl_pool = if let Some(ref connector) = self.secure_connector {
            Some(ConnectionPool::new(
                connector.create(cfg).await?.into(),
                self.conn_lifetime,
                self.conn_keep_alive,
                self.limit,
                cfg,
            ))
        } else {
            None
        };
        Ok(ConnectorService { tcp_pool, ssl_pool })
    }
}

/// Manages http client network connectivity.
#[derive(Debug)]
pub struct ConnectorService {
    tcp_pool: ConnectionPool,
    ssl_pool: Option<ConnectionPool>,
}

impl Service<Connect> for ConnectorService {
    type Response = Connection;
    type Error = ConnectError;

    #[inline]
    async fn ready(&self, ctx: ServiceCtx<'_, Self>) -> Result<(), Self::Error> {
        if let Some(ref ssl_pool) = self.ssl_pool {
            let (r1, r2) = join(ctx.ready(&self.tcp_pool), ctx.ready(ssl_pool)).await;
            r1?;
            r2
        } else {
            ctx.ready(&self.tcp_pool).await
        }
    }

    #[inline]
    fn poll(&self, cx: &mut Context<'_>) -> Result<(), Self::Error> {
        self.tcp_pool.poll(cx)?;
        if let Some(ref ssl_pool) = self.ssl_pool {
            ssl_pool.poll(cx)?;
        }
        Ok(())
    }

    async fn shutdown(&self) {
        self.tcp_pool.shutdown().await;
        if let Some(ref ssl_pool) = self.ssl_pool {
            ssl_pool.shutdown().await;
        }
    }

    async fn call(
        &self,
        req: Connect,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        match req.uri.scheme_str() {
            Some("https") | Some("wss") => {
                if let Some(ref conn) = self.ssl_pool {
                    ctx.call(conn, req).await
                } else {
                    Err(ConnectError::SslIsNotSupported)
                }
            }
            _ => ctx.call(&self.tcp_pool, req).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{io::SharedConfig, service::Pipeline, util::lazy};

    #[crate::rt_test]
    async fn test_readiness() {
        let conn = Pipeline::new(
            Connector::default()
                .create(SharedConfig::default())
                .await
                .unwrap(),
        )
        .bind();
        assert!(lazy(|cx| conn.poll_ready(cx).is_ready()).await);
        assert!(lazy(|cx| conn.poll_shutdown(cx).is_ready()).await);
    }
}
