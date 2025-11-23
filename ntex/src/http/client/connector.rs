use std::{fmt, task::Context, time::Duration};

use ntex_h2::{self as h2};

use crate::connect::{Connect as TcpConnect, Connector as TcpConnector};
use crate::service::{Service, ServiceCtx, apply_fn, boxed};
use crate::time::{Millis, Seconds};
use crate::util::{join, timeout::TimeoutError, timeout::TimeoutService};
use crate::{http::Uri, io::IoBoxed};

use super::{Connect, connection::Connection, error::ConnectError, pool::ConnectionPool};

#[cfg(feature = "openssl")]
use tls_openssl::ssl::SslConnector as OpensslConnector;

#[cfg(feature = "rustls")]
use tls_rustls::ClientConfig;

type BoxedConnector = boxed::BoxService<TcpConnect<Uri>, IoBoxed, ConnectError>;

#[derive(Debug)]
/// Manages http client network connectivity.
///
/// The `Connector` type uses a builder-like combinator pattern for service
/// construction that finishes by calling the `.finish()` method.
///
/// ```rust,no_run
/// use ntex::http::client::Connector;
/// use ntex::time::Millis;
///
/// let connector = Connector::default()
///      .timeout(Millis(5_000))
///      .finish();
/// ```
pub struct Connector {
    timeout: Millis,
    conn_lifetime: Duration,
    conn_keep_alive: Duration,
    limit: usize,
    h2config: h2::Config,
    connector: BoxedConnector,
    ssl_connector: Option<BoxedConnector>,
}

impl Default for Connector {
    fn default() -> Self {
        Connector::new()
    }
}

impl Connector {
    pub fn new() -> Connector {
        let conn = Connector {
            connector: boxed::service(
                TcpConnector::new()
                    .map(IoBoxed::from)
                    .map_err(ConnectError::from),
            ),
            ssl_connector: None,
            timeout: Millis(1_000),
            conn_lifetime: Duration::from_secs(75),
            conn_keep_alive: Duration::from_secs(15),
            limit: 100,
            h2config: h2::Config::client(),
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
    /// Connection timeout.
    ///
    /// i.e. max time to connect to remote host including dns name resolution.
    /// Set to 1 second by default.
    pub fn timeout<T: Into<Millis>>(mut self, timeout: T) -> Self {
        self.timeout = timeout.into();
        self
    }

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
    pub fn keep_alive(mut self, dur: Seconds) -> Self {
        self.conn_keep_alive = dur.into();
        self
    }

    /// Set max lifetime period for connection.
    ///
    /// Connection lifetime is max lifetime of any opened connection
    /// until it is closed regardless of keep-alive period.
    /// Default lifetime period is 75 seconds.
    pub fn lifetime(mut self, dur: Seconds) -> Self {
        self.conn_lifetime = dur.into();
        self
    }

    #[doc(hidden)]
    /// Configure http2 connection settings
    pub fn configure_http2<O, R>(self, f: O) -> Self
    where
        O: FnOnce(&h2::Config) -> R,
    {
        let _ = f(&self.h2config);
        self
    }

    /// Use custom connector to open un-secured connections.
    pub fn connector<T>(mut self, connector: T) -> Self
    where
        T: Service<TcpConnect<Uri>, Error = crate::connect::ConnectError> + 'static,
        IoBoxed: From<T::Response>,
    {
        self.connector =
            boxed::service(connector.map(IoBoxed::from).map_err(ConnectError::from));
        self
    }

    /// Use custom connector to open secure connections.
    pub fn secure_connector<T>(mut self, connector: T) -> Self
    where
        T: Service<TcpConnect<Uri>, Error = crate::connect::ConnectError> + 'static,
        IoBoxed: From<T::Response>,
    {
        self.ssl_connector = Some(boxed::service(
            connector.map(IoBoxed::from).map_err(ConnectError::from),
        ));
        self
    }

    /// Finish configuration process and create connector service.
    /// The Connector builder always concludes by calling `finish()` last in
    /// its combinator chain.
    pub fn finish(
        self,
    ) -> impl Service<Connect, Response = Connection, Error = ConnectError> + fmt::Debug
    {
        let tcp_service = connector(self.connector, self.timeout);

        let ssl_pool = if let Some(ssl_connector) = self.ssl_connector {
            let srv = connector(ssl_connector, self.timeout);
            Some(ConnectionPool::new(
                srv,
                self.conn_lifetime,
                self.conn_keep_alive,
                self.limit,
                self.h2config.clone(),
            ))
        } else {
            None
        };

        InnerConnector {
            tcp_pool: ConnectionPool::new(
                tcp_service,
                self.conn_lifetime,
                self.conn_keep_alive,
                self.limit,
                self.h2config.clone(),
            ),
            ssl_pool,
        }
    }
}

fn connector(
    connector: BoxedConnector,
    timeout: Millis,
) -> impl Service<Connect, Response = IoBoxed, Error = ConnectError> + fmt::Debug {
    TimeoutService::new(
        timeout,
        apply_fn(connector, |msg: Connect, svc| async move {
            svc.call(TcpConnect::new(msg.uri).set_addr(msg.addr)).await
        })
        .map_err(ConnectError::from),
    )
    .map_err(|e| match e {
        TimeoutError::Service(e) => e,
        TimeoutError::Timeout => ConnectError::Timeout,
    })
}

#[derive(Debug)]
struct InnerConnector<T> {
    tcp_pool: ConnectionPool<T>,
    ssl_pool: Option<ConnectionPool<T>>,
}

impl<T> Service<Connect> for InnerConnector<T>
where
    T: Service<Connect, Response = IoBoxed, Error = ConnectError> + 'static,
{
    type Response = <ConnectionPool<T> as Service<Connect>>::Response;
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
    use crate::{service::Pipeline, util::lazy};

    #[crate::rt_test]
    async fn test_readiness() {
        let conn = Pipeline::new(Connector::default().finish()).bind();
        assert!(lazy(|cx| conn.poll_ready(cx).is_ready()).await);
        assert!(lazy(|cx| conn.poll_shutdown(cx).is_ready()).await);
    }
}
