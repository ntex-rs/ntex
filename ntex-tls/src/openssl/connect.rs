//! Client service that composes transport establishment with an OpenSSL handshake.
use std::io;

use ntex_error::Error;
use ntex_io::{Filter, Io, Layer};
use ntex_net::connect::{Address, Connect, ConnectError, Connector};
use ntex_service::{Ctx, IntoService, Service, cfg::SharedCfg};
use tls_openssl::ssl::SslConnector as OpensslConnector;

use crate::{TlsConfig, openssl::SslFilter};

#[derive(Clone, Debug)]
/// Opens a transport and upgrades it to client-side TLS using OpenSSL.
///
/// Accepts [`Connect`] requests with [`SharedCfg`] service state. Binding,
/// readiness, and shutdown are forwarded to the underlying transport connector.
/// The [`TlsConfig`] timeout covers TLS negotiation after transport establishment,
/// not DNS resolution or TCP connection setup.
///
/// The service strips ports and IPv6 brackets before configuring the OpenSSL
/// session's host. Verification, client identity, and ALPN otherwise follow the
/// supplied OpenSSL connector.
pub struct SslConnector<S> {
    /// Service responsible for producing the underlying I/O.
    svc: S,
    /// OpenSSL policy used to configure each client session.
    openssl: OpensslConnector,
}

impl<A: Address> SslConnector<Connector<A>> {
    /// Creates a TLS connector using the default network connector.
    pub fn new(openssl: OpensslConnector) -> Self {
        SslConnector {
            openssl,
            svc: Connector::default(),
        }
    }

    /// Replaces the default transport connector without changing the TLS policy.
    ///
    /// The replacement receives the original request and shared service state.
    /// Any filters on its returned I/O remain below TLS.
    pub fn connector<F, S>(self, f: impl IntoService<S, SharedCfg, Connect<A>>) -> SslConnector<S>
    where
        S: Service<SharedCfg, Connect<A>, Res = Io<F>, Error = Error<ConnectError>>,
    {
        SslConnector {
            svc: f.into_service(),
            openssl: self.openssl,
        }
    }
}

impl<S> SslConnector<S> {
    /// Completes a client handshake over existing I/O using this connector's TLS policy.
    ///
    /// Does not call the underlying transport service. `host` is passed directly
    /// to OpenSSL for server-name and verification configuration: supply a DNS
    /// name or IP address without a port or IPv6 brackets. Existing filters are
    /// preserved, and the handshake timeout comes from `cfg`'s [`TlsConfig`].
    ///
    /// # Errors
    ///
    /// OpenSSL configuration failures become [`io::ErrorKind::InvalidInput`].
    /// Configuration, handshake, timeout, and transport failures are wrapped as
    /// [`ConnectError`] and tagged with the TLS configuration's service identity.
    pub async fn connect<F: Filter>(
        &self,
        io: Io<F>,
        host: &str,
        cfg: &SharedCfg,
    ) -> Result<Io<Layer<SslFilter, F>>, Error<ConnectError>> {
        let cfg = cfg.get::<TlsConfig>();
        crate::utils::connect(&cfg, host, async {
            let ssl = self
                .openssl
                .configure()
                .and_then(|config| config.into_ssl(host))
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
            super::handshake(io, ssl, false).await
        })
        .await
    }
}

impl<F: Filter, A: Address, S> Service<SharedCfg, Connect<A>> for SslConnector<S>
where
    S: Service<SharedCfg, Connect<A>, Res = Io<F>, Error = Error<ConnectError>>,
{
    type Res = Io<Layer<SslFilter, F>>;
    type Error = Error<ConnectError>;

    /// Establishes transport, then upgrades it using the normalized request host.
    async fn call(
        &self,
        req: Connect<A>,
        ctx: Ctx<'_, Self, SharedCfg>,
    ) -> Result<Self::Res, Self::Error> {
        let host = crate::utils::server_name(req.host()).to_string();
        let io = ctx.call(&self.svc, req).await?;
        self.connect(io, &host, ctx.st()).await
    }

    ntex_service::forward!(SharedCfg, svc);
}

#[cfg(test)]
mod tests {
    use super::*;

    use ntex_service::Pipeline;
    use tls_openssl::ssl::SslMethod;

    /// STATEMENT: An OpenSSL connector is ready but reports failure against a non-TLS peer.
    ///
    /// HOW:
    /// - Construct and clone the connector, check its debug name, and await readiness.
    /// - Connect to a local service that drops the transport and assert the call fails.
    #[ntex::test]
    async fn test_openssl_connect() {
        let server = ntex::server::test_server(async || {
            ntex::service::fn_service(async |_| Ok::<_, ()>(()))
        });

        let ssl = OpensslConnector::builder(SslMethod::tls()).unwrap();
        let _: SslConnector<Connector<&'static str>> = SslConnector::new(ssl.build());
        let ssl = OpensslConnector::builder(SslMethod::tls()).unwrap();
        let svc = SslConnector::new(ssl.build()).clone();
        assert!(format!("{svc:?}").contains("SslConnector"));

        let srv = Pipeline::new(SharedCfg::default(), svc);
        // always ready
        assert!(srv.ready().await.is_ok());
        let result = srv
            .call(Connect::new("").set_addr(Some(server.addr())))
            .await;
        assert!(result.is_err());
    }
}
