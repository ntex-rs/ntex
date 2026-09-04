use crate::connect::{self, Connect as TcpConnect, Connector as TcpConnector};
use crate::service::{Identity, IntoService, Middleware, Pipeline, Service, Stack, apply_fn};
use crate::{SharedCfg, error::Error, http::Uri, io::IoBoxed};

use super::connector::Connector;
use super::error::{ClientError, ConnectError};
use super::pool::ConnectionPool;
use super::service::{ServiceRequest, ServiceResponse};
use super::{Client, ClientConfig, Connect, ConnectorPipeline, sender::Sender};

#[cfg(feature = "openssl")]
use tls_openssl::ssl::SslConnector as OpensslConnector;

#[cfg(feature = "rustls")]
use tls_rustls::ClientConfig as RustlsClientConfig;

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

        let builder = ClientBuilder {
            svc,
            secure_svc: None,
            middleware: Identity,
        };

        #[cfg(feature = "openssl")]
        {
            use tls_openssl::ssl::SslMethod;

            let mut ssl = OpensslConnector::builder(SslMethod::tls()).unwrap();
            let _ = ssl
                .set_alpn_protos(b"\x02h2\x08http/1.1")
                .map_err(|e| log::error!("Cannot set ALPN protocol: {e:?}"));
            ssl.set_verify(tls_openssl::ssl::SslVerifyMode::NONE);

            builder.openssl(ssl.build())
        }
        #[cfg(all(not(feature = "openssl"), feature = "rustls"))]
        {
            use tls_rustls::RootCertStore;

            let protos = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
            let cert_store =
                RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let mut config = RustlsClientConfig::builder()
                .with_root_certificates(cert_store)
                .with_no_client_auth();
            config.alpn_protocols = protos;
            builder.rustls(config)
        }
        #[cfg(not(any(feature = "openssl", feature = "rustls")))]
        {
            builder
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
    pub fn rustls(self, config: RustlsClientConfig) -> Self {
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
    ///         .build(SharedCfg::default());
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

        let connector = Connector {
            tcp_pool: ConnectionPool::new(self.svc, config.clone()),
            ssl_pool: self
                .secure_svc
                .map(|svc| ConnectionPool::new(svc, config.clone())),
        };
        let svc = self.middleware.create(&cfg, Sender::new(connector));

        Client::with_service(config, Pipeline::new(cfg, svc))
    }
}
