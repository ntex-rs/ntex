use crate::connect::{self, Connect as TcpConnect, Connector as TcpConnector};
use crate::service::{Identity, IntoService, Middleware, Pipeline, Service, Stack, apply_fn};
use crate::{SharedCfg, error::Error, http::Uri, io::IoBoxed};

use super::connector::Connector;
use super::error::{ClientError, ConnectError};
use super::pool::ConnectionPool;
use super::service::{ServiceRequest, ServiceResponse};
use super::{Client, ClientConfig, Connect, ConnectorPipeline, sender::Sender};

/// An HTTP Client builder.
///
/// This type can be used to construct an instance of `Client` through a
/// builder-like pattern.
#[derive(Debug)]
pub struct ClientBuilder<M = Identity> {
    middleware: M,
    svc: ConnectorPipeline,
    secure_svc: Option<ConnectorPipeline>,
}

impl Default for ClientBuilder<Identity> {
    fn default() -> Self {
        Self::new()
    }
}

impl ClientBuilder<Identity> {
    #[must_use]
    /// Create new client builder instance.
    pub fn new() -> Self {
        let svc = ConnectorPipeline::new(
            apply_fn(TcpConnector::new(), async move |msg: Connect, svc| {
                svc.call(TcpConnect::new(msg.uri).set_addr(msg.addr)).await
            })
            .map(IoBoxed::from)
            .map_err(|e| e.map(ConnectError::from)),
        );

        let secure_svc = {
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
                None
            }
        };

        ClientBuilder {
            svc,
            secure_svc,
            middleware: Identity,
        }
    }
}

impl<M> ClientBuilder<M> {
    #[must_use]
    #[cfg(feature = "openssl")]
    /// Use openssl connector for secured connections.
    pub fn openssl(self, config: OpensslConnector) -> Self {
        use crate::connect::openssl::SslConnector;

        self.secure_connector(SslConnector::new(config))
    }

    #[must_use]
    #[cfg(feature = "rustls")]
    /// Use rustls connector for secured connections.
    pub fn rustls(self, config: ClientConfig) -> Self {
        use crate::connect::rustls::TlsConnector;

        self.secure_connector(TlsConnector::new(config))
    }

    #[must_use]
    /// Use custom connector to open un-secured connections.
    pub fn connector<T>(mut self, f: impl IntoService<T, SharedCfg, TcpConnect<Uri>>) -> Self
    where
        T: Service<SharedCfg, TcpConnect<Uri>, Error = Error<connect::ConnectError>> + 'static,
        IoBoxed: From<T::Res>,
    {
        self.svc = ConnectorPipeline::new(
            apply_fn(f.into_service(), async move |msg: Connect, svc| {
                svc.call(TcpConnect::new(msg.uri).set_addr(msg.addr)).await
            })
            .map(IoBoxed::from)
            .map_err(|e| e.map(ConnectError::from)),
        );
        self
    }

    #[must_use]
    /// Use custom connector to open secure connections.
    pub fn secure_connector<T>(mut self, f: impl IntoService<T, SharedCfg, TcpConnect<Uri>>) -> Self
    where
        T: Service<SharedCfg, TcpConnect<Uri>, Error = Error<connect::ConnectError>> + 'static,
        IoBoxed: From<T::Res>,
    {
        self.secure_svc = Some(ConnectorPipeline::new(
            apply_fn(f.into_service(), async move |msg: Connect, svc| {
                svc.call(TcpConnect::new(msg.uri).set_addr(msg.addr)).await
            })
            .map(IoBoxed::from)
            .map_err(|e| e.map(ConnectError::from)),
        ));
        self
    }

    #[must_use]
    /// Apply middleware.
    ///
    /// Use middleware when you need to read or modify *every* request or
    /// response in some way.
    ///
    /// ```rust
    /// use ntex::client::{Client, ServiceRequest};
    /// use ntex::service::{fn_layer, cfg::SharedCfg};
    ///
    /// #[ntex::main]
    /// async fn main() {
    ///     let client = Client::builder()
    ///         .middleware(fn_layer(
    ///             async move |mut req: ServiceRequest, svc| {
    ///                 println!("{:?}", req.head().uri);
    ///                 svc.call(req).await
    ///             }
    ///         ))
    ///         .build(SharedCfg::default())
    ///         .await;
    /// }
    /// ```
    pub fn middleware<U>(self, mw: U) -> ClientBuilder<Stack<U, M>> {
        ClientBuilder {
            middleware: Stack::new(mw, self.middleware),
            svc: self.svc,
            secure_svc: self.secure_svc,
        }
    }

    /// Finish build process and create `Client` instance.
    pub fn build(self, cfg: impl Into<SharedCfg>) -> Client
    where
        M: Middleware<Sender, SharedCfg>,
        M::Service: Service<SharedCfg, ServiceRequest, Res = ServiceResponse, Error = Error<ClientError>>
            + 'static,
    {
        let cfg = cfg.into();
        let config = cfg.get::<ClientConfig>();

        let ssl_pool = if let Some(svc) = self.secure_svc {
            Some(ConnectionPool::new(svc, config.clone()))
        } else {
            None
        };
        let tcp_pool = ConnectionPool::new(self.svc, config.clone());
        let connector = Connector { tcp_pool, ssl_pool };
        let svc = self.middleware.create(Sender::new(connector), &cfg);

        Client::with_service(config, Pipeline::with_st(cfg.into(), svc))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[crate::rt_test]
    async fn basics() {
        let builder = ClientBuilder::new()
            .disable_timeout()
            .disable_redirects()
            .max_redirects(10)
            .no_default_headers();
        assert!(!builder.allow_redirects);
        assert!(!builder.config.default_headers);
        assert_eq!(builder.max_redirects, 10);
    }

    #[crate::rt_test]
    async fn response_payload_limit() {
        let builder = ClientBuilder::default();
        assert_eq!(builder.config.pl_limit, 262_144);

        let builder = builder.response_payload_limit(10);
        assert_eq!(builder.config.pl_limit, 10);
    }

    #[crate::rt_test]
    async fn response_payload_timeout() {
        let builder = ClientBuilder::default();
        assert_eq!(builder.config.pl_timeout, Millis(10_000));

        let builder = builder.response_payload_timeout(Millis(10));
        assert_eq!(builder.config.pl_timeout, Millis(10));
    }

    #[crate::rt_test]
    async fn valid_header_name() {
        let builder = ClientBuilder::new().header("Content-Length", 1);
        assert!(builder.config.headers.contains_key("Content-Length"));
    }

    #[crate::rt_test]
    async fn invalid_header_name() {
        let builder = ClientBuilder::new().header("no valid header name", 1);
        assert!(!builder.config.headers.contains_key("no valid header name"));
    }

    #[crate::rt_test]
    async fn valid_header_value() {
        let valid_header_value = HeaderValue::from(1234);
        let builder = ClientBuilder::new().header("Content-Length", &valid_header_value);
        assert_eq!(
            builder.config.headers.get("Content-Length"),
            Some(&valid_header_value)
        );
    }

    #[crate::rt_test]
    async fn invalid_header_value() {
        let builder = ClientBuilder::new().header("Content-Length", "\n");
        assert!(!builder.config.headers.contains_key("Content-Length"));
    }

    #[crate::rt_test]
    async fn client_basic_auth() {
        let client = ClientBuilder::new().basic_auth("username", Some("password"));
        assert_eq!(
            client
                .config
                .headers
                .get(header::AUTHORIZATION)
                .unwrap()
                .to_str()
                .unwrap(),
            "Basic dXNlcm5hbWU6cGFzc3dvcmQ="
        );

        let client = ClientBuilder::new().basic_auth("username", None);
        assert_eq!(
            client
                .config
                .headers
                .get(header::AUTHORIZATION)
                .unwrap()
                .to_str()
                .unwrap(),
            "Basic dXNlcm5hbWU6"
        );
    }

    #[crate::rt_test]
    async fn client_bearer_auth() {
        let client = ClientBuilder::new().bearer_auth("someS3cr3tAutht0k3n");
        assert_eq!(
            client
                .config
                .headers
                .get(header::AUTHORIZATION)
                .unwrap()
                .to_str()
                .unwrap(),
            "Bearer someS3cr3tAutht0k3n"
        );
    }
}
