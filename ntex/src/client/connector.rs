use std::{error::Error as StdError, task::Context, time::Duration};

use crate::connect::{Connect as TcpConnect, Connector as TcpConnector};
use crate::error::{Error, ErrorMapping, with_service};
use crate::service::{Ctx, ReadyCtx, Service, ServiceFactory, apply_fn_factory, boxed};
use crate::{SharedCfg, http::Uri, io::IoBoxed, time::Seconds, util::join};

use super::error::{ClientError, ConnectError};
use super::{Connect, Connection, pool::ConnectionPool};

#[cfg(feature = "openssl")]
use tls_openssl::ssl::SslConnector as OpensslConnector;

#[cfg(feature = "rustls")]
use tls_rustls::ClientConfig;

type BoxedConnector = boxed::BoxServiceFactory<
    (),
    Connect,
    IoBoxed,
    Error<ConnectError>,
    SharedCfg,
    Box<dyn StdError>,
>;

#[derive(Debug)]
/// Manages http client network connectivity.
///
/// The `Connector` type uses a builder-like combinator pattern for service
/// construction that finishes by calling the `.finish()` method.
///
/// ```rust,no_run
/// use ntex::client::Connector;
///
/// let connector = Connector::default()
///      .keep_alive(5_000);
/// ```
pub struct Connector {
    conn_lifetime: Duration,
    conn_keep_alive: Duration,
    limit: usize,
    svc: BoxedConnector,
    secure_svc: Option<BoxedConnector>,
}

impl Default for Connector {
    fn default() -> Self {
        Connector::new()
    }
}

impl Connector {
    pub fn new() -> Connector {
        let conn = Connector {
            svc: boxed::factory(
                apply_fn_factory(TcpConnector::new(), async move |msg: Connect, svc| {
                    svc.call(TcpConnect::new(msg.uri).set_addr(msg.addr)).await
                })
                .map(IoBoxed::from)
                .map_err(|e| e.map(ConnectError::from))
                .map_init_err(|e| Box::new(e) as Box<dyn StdError>),
            ),
            secure_svc: None,
            conn_lifetime: Duration::from_secs(75),
            conn_keep_alive: Duration::from_secs(15),
            limit: 8,
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
    #[must_use]
    #[cfg(feature = "openssl")]
    /// Use openssl connector for secured connections.
    pub fn openssl(self, connector: OpensslConnector) -> Self {
        use crate::connect::openssl::SslConnector;

        self.secure_connector(SslConnector::new(connector))
    }

    #[must_use]
    #[cfg(feature = "rustls")]
    /// Use rustls connector for secured connections.
    pub fn rustls(self, connector: ClientConfig) -> Self {
        use crate::connect::rustls::TlsConnector;

        self.secure_connector(TlsConnector::new(connector))
    }

    #[must_use]
    /// Set total number of simultaneous connections per type of scheme.
    ///
    /// If limit is 0, the connector has no limit.
    /// The default limit size is 8.
    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    #[must_use]
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

    #[must_use]
    /// Set max lifetime period for connection.
    ///
    /// Connection lifetime is max lifetime of any opened connection
    /// until it is closed regardless of keep-alive period.
    /// Default lifetime period is 75 seconds.
    pub fn lifetime<T: Into<Seconds>>(mut self, dur: T) -> Self {
        self.conn_lifetime = dur.into().into();
        self
    }

    #[must_use]
    /// Use custom connector to open un-secured connections.
    pub fn connector<T>(mut self, connector: T) -> Self
    where
        T: ServiceFactory<
                (),
                TcpConnect<Uri>,
                Error = Error<crate::connect::ConnectError>,
                InitCfg = SharedCfg,
            > + 'static,
        T::InitError: StdError,
        IoBoxed: From<T::Res>,
    {
        self.svc = boxed::factory(
            apply_fn_factory(connector, async move |msg: Connect, svc| {
                svc.call(TcpConnect::new(msg.uri).set_addr(msg.addr)).await
            })
            .map(IoBoxed::from)
            .map_err(|e| e.map(ConnectError::from))
            .map_init_err(|e| Box::new(e) as Box<dyn StdError>),
        );
        self
    }

    #[must_use]
    /// Use custom connector to open secure connections.
    pub fn secure_connector<T>(mut self, connector: T) -> Self
    where
        T: ServiceFactory<
                (),
                TcpConnect<Uri>,
                Error = Error<crate::connect::ConnectError>,
                InitCfg = SharedCfg,
            > + 'static,
        T::InitError: StdError,
        IoBoxed: From<T::Res>,
    {
        self.secure_svc = Some(boxed::factory(
            apply_fn_factory(connector, async move |msg: Connect, svc| {
                svc.call(TcpConnect::new(msg.uri).set_addr(msg.addr)).await
            })
            .map(IoBoxed::from)
            .map_err(|e| e.map(ConnectError::from))
            .map_init_err(|e| Box::new(e) as Box<dyn StdError>),
        ));
        self
    }
}

impl ServiceFactory<(), Connect> for Connector {
    type Res = Connection;
    type Error = Error<ClientError>;
    type Service = ConnectorService;
    type InitCfg = SharedCfg;
    type InitError = Box<dyn StdError>;

    async fn create(&self, cfg: &SharedCfg) -> Result<Self::Service, Self::InitError> {
        let ssl_pool = if let Some(ref svc) = self.secure_svc {
            Some(ConnectionPool::new(
                svc.create(cfg).await?.into(),
                self.conn_lifetime,
                self.conn_keep_alive,
                self.limit,
                cfg.clone(),
            ))
        } else {
            None
        };
        let tcp_pool = ConnectionPool::new(
            self.svc.create(cfg).await?.into(),
            self.conn_lifetime,
            self.conn_keep_alive,
            self.limit,
            cfg.clone(),
        );
        Ok(ConnectorService {
            tcp_pool,
            ssl_pool,
            cfg: cfg.clone(),
        })
    }
}

/// Manages http client network connectivity.
#[derive(Clone, Debug)]
pub struct ConnectorService {
    cfg: SharedCfg,
    tcp_pool: ConnectionPool,
    ssl_pool: Option<ConnectionPool>,
}

impl Service<(), Connect> for ConnectorService {
    type Res = Connection;
    type Error = Error<ClientError>;

    #[inline]
    async fn ready(&self, ctx: ReadyCtx<'_, Self, ()>) -> Result<(), Self::Error> {
        if let Some(ref ssl_pool) = self.ssl_pool {
            let (r1, r2) = join(ctx.ready(&self.tcp_pool), ctx.ready(ssl_pool)).await;
            r1.into_error()?;
            r2.into_error()
        } else {
            ctx.ready(&self.tcp_pool).await.into_error()
        }
    }

    #[inline]
    fn poll(&self, cx: &mut Context<'_>) -> Result<(), Self::Error> {
        self.tcp_pool.poll(cx).into_error()?;
        if let Some(ref ssl_pool) = self.ssl_pool {
            ssl_pool.poll(cx).into_error()?;
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
        ctx: Ctx<'_, Self, ()>,
    ) -> Result<Self::Res, Self::Error> {
        with_service(self.cfg.service(), async {
            match req.uri.scheme_str() {
                Some("https" | "wss") => {
                    if let Some(ref conn) = self.ssl_pool {
                        ctx.call(conn, req).await.into_error()
                    } else {
                        Err(Error::from(ClientError::from(
                            ConnectError::SslIsNotSupported,
                        )))
                    }
                }
                _ => ctx.call(&self.tcp_pool, req).await.into_error(),
            }
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{service::Pipeline, util::lazy};

    #[crate::rt_test]
    async fn test_readiness() {
        let conn = Pipeline::new(
            Connector::default()
                .create(SharedCfg::default())
                .await
                .unwrap(),
        )
        .bind();
        assert!(lazy(|cx| conn.poll_ready(cx).is_ready()).await);
        assert!(lazy(|cx| conn.poll_shutdown(cx).is_ready()).await);
    }
}
