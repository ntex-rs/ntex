use std::{rc::Rc, task::Context, task::Poll, time::Duration};

use crate::connect::{Connect as TcpConnect, Connector as TcpConnector};
use crate::http::Uri;
use crate::io::{Filter, Io, IoBoxed};
use crate::service::{apply_fn, boxed, Service};
use crate::time::{Millis, Seconds};
use crate::util::timeout::{TimeoutError, TimeoutService};
use crate::util::{Either, Ready};

use super::connection::Connection;
use super::error::ConnectError;
use super::pool::ConnectionPool;
use super::Connect;

#[cfg(feature = "openssl")]
use crate::connect::openssl::SslConnector;

#[cfg(feature = "rustls")]
use crate::connect::rustls::ClientConfig;

type BoxedConnector = boxed::BoxService<TcpConnect<Uri>, IoBoxed, ConnectError>;

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
    disconnect_timeout: Millis,
    limit: usize,
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
                    .map(|io| io.into_boxed())
                    .map_err(ConnectError::from),
            ),
            ssl_connector: None,
            timeout: Millis(1_000),
            conn_lifetime: Duration::from_secs(75),
            conn_keep_alive: Duration::from_secs(15),
            disconnect_timeout: Millis(3_000),
            limit: 100,
        };

        #[cfg(feature = "openssl")]
        {
            use crate::connect::openssl::SslMethod;

            let mut ssl = SslConnector::builder(SslMethod::tls()).unwrap();
            let _ = ssl
                .set_alpn_protos(b"\x02h2\x08http/1.1")
                .map_err(|e| error!("Cannot set ALPN protocol: {:?}", e));
            conn.openssl(ssl.build())
        }
        #[cfg(all(not(feature = "openssl"), feature = "rustls"))]
        {
            use rust_tls::{OwnedTrustAnchor, RootCertStore};

            let protos = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
            let mut cert_store = RootCertStore::empty();
            cert_store.add_server_trust_anchors(
                webpki_roots::TLS_SERVER_ROOTS.0.iter().map(|ta| {
                    OwnedTrustAnchor::from_subject_spki_name_constraints(
                        ta.subject,
                        ta.spki,
                        ta.name_constraints,
                    )
                }),
            );
            let mut config = ClientConfig::builder()
                .with_safe_defaults()
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
    pub fn openssl(self, connector: SslConnector) -> Self {
        use crate::connect::openssl::Connector;

        self.secure_connector(Connector::new(connector))
    }

    #[cfg(feature = "rustls")]
    /// Use rustls connector for secured connections.
    pub fn rustls(self, connector: ClientConfig) -> Self {
        use crate::connect::rustls::Connector;

        self.secure_connector(Connector::new(connector))
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

    /// Set server connection disconnect timeout.
    ///
    /// Defines a timeout for disconnect connection. If a disconnect procedure does not complete
    /// within this time, the socket get dropped. This timeout affects only secure connections.
    ///
    /// To disable timeout set value to 0.
    ///
    /// By default disconnect timeout is set to 3 seconds.
    pub fn disconnect_timeout<T: Into<Millis>>(mut self, timeout: T) -> Self {
        self.disconnect_timeout = timeout.into();
        self
    }

    /// Use custom connector to open un-secured connections.
    pub fn connector<T, F>(mut self, connector: T) -> Self
    where
        T: Service<
                Request = TcpConnect<Uri>,
                Response = Io<F>,
                Error = crate::connect::ConnectError,
            > + 'static,
        F: Filter,
    {
        self.connector = boxed::service(
            connector
                .map(|io| io.into_boxed())
                .map_err(ConnectError::from),
        );
        self
    }

    /// Use custom connector to open secure connections.
    pub fn secure_connector<T, F>(mut self, connector: T) -> Self
    where
        T: Service<
                Request = TcpConnect<Uri>,
                Response = Io<F>,
                Error = crate::connect::ConnectError,
            > + 'static,
        F: Filter,
    {
        self.ssl_connector = Some(boxed::service(
            connector
                .map(|io| io.into_boxed())
                .map_err(ConnectError::from),
        ));
        self
    }

    /// Finish configuration process and create connector service.
    /// The Connector builder always concludes by calling `finish()` last in
    /// its combinator chain.
    pub fn finish(
        self,
    ) -> impl Service<Request = Connect, Response = Connection, Error = ConnectError> + Clone
    {
        let tcp_service =
            connector(self.connector, self.timeout, self.disconnect_timeout);

        let ssl_pool = if let Some(ssl_connector) = self.ssl_connector {
            let srv = connector(ssl_connector, self.timeout, self.disconnect_timeout);
            Some(ConnectionPool::new(
                srv,
                self.conn_lifetime,
                self.conn_keep_alive,
                self.disconnect_timeout,
                self.limit,
            ))
        } else {
            None
        };

        Rc::new(InnerConnector {
            tcp_pool: ConnectionPool::new(
                tcp_service,
                self.conn_lifetime,
                self.conn_keep_alive,
                self.disconnect_timeout,
                self.limit,
            ),
            ssl_pool,
        })
    }
}

fn connector(
    connector: BoxedConnector,
    timeout: Millis,
    disconnect_timeout: Millis,
) -> impl Service<
    Request = Connect,
    Response = IoBoxed,
    Error = ConnectError,
    Future = impl Unpin,
> + Unpin {
    TimeoutService::new(
        timeout,
        apply_fn(connector, |msg: Connect, srv| {
            srv.call(TcpConnect::new(msg.uri).set_addr(msg.addr))
        })
        .map(move |io: IoBoxed| {
            io.set_disconnect_timeout(disconnect_timeout);
            io
        })
        .map_err(ConnectError::from),
    )
    .map_err(|e| match e {
        TimeoutError::Service(e) => e,
        TimeoutError::Timeout => ConnectError::Timeout,
    })
}

struct InnerConnector<T> {
    tcp_pool: ConnectionPool<T>,
    ssl_pool: Option<ConnectionPool<T>>,
}

impl<T> Service for InnerConnector<T>
where
    T: Service<Request = Connect, Response = IoBoxed, Error = ConnectError>
        + Unpin
        + 'static,
    T::Future: Unpin,
{
    type Request = Connect;
    type Response = <ConnectionPool<T> as Service>::Response;
    type Error = ConnectError;
    type Future = Either<
        <ConnectionPool<T> as Service>::Future,
        Ready<Self::Response, Self::Error>,
    >;

    #[inline]
    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let ready = self.tcp_pool.poll_ready(cx)?.is_ready();
        let ready = if let Some(ref ssl_pool) = self.ssl_pool {
            ssl_pool.poll_ready(cx)?.is_ready() && ready
        } else {
            ready
        };
        if ready {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }

    #[inline]
    fn poll_shutdown(&self, cx: &mut Context<'_>, is_error: bool) -> Poll<()> {
        let tcp_ready = self.tcp_pool.poll_shutdown(cx, is_error).is_ready();
        let ssl_ready = self
            .ssl_pool
            .as_ref()
            .map(|pool| pool.poll_shutdown(cx, is_error).is_ready())
            .unwrap_or(true);
        if tcp_ready && ssl_ready {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }

    fn call(&self, req: Connect) -> Self::Future {
        match req.uri.scheme_str() {
            Some("https") | Some("wss") => {
                if let Some(ref conn) = self.ssl_pool {
                    Either::Left(conn.call(req))
                } else {
                    Either::Right(Ready::Err(ConnectError::SslIsNotSupported))
                }
            }
            _ => Either::Left(self.tcp_pool.call(req)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::lazy;

    #[crate::rt_test]
    async fn test_readiness() {
        let conn = Connector::default().finish();
        assert!(lazy(|cx| conn.poll_ready(cx).is_ready()).await);
        assert!(lazy(|cx| conn.poll_shutdown(cx, true).is_ready()).await);
    }
}
