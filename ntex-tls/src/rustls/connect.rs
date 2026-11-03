//! Client service that composes transport establishment with a rustls handshake.
use std::{fmt, io, sync::Arc};

use ntex_error::Error;
use ntex_io::{Filter, Io, Layer};
use ntex_net::connect::{Address, Connect, ConnectError, Connector};
use ntex_service::{Ctx, IntoService, Service, cfg::SharedCfg};
use tls_rustls::{ClientConfig, pki_types::ServerName};

use crate::{TlsConfig, rustls::TlsClientFilter};

/// Opens a transport and upgrades it to client-side TLS using rustls.
///
/// The service accepts [`Connect`] requests with [`SharedCfg`] state. It strips
/// ports and IPv6 brackets from the request host for rustls server-name handling.
/// The supplied [`ClientConfig`] determines trust, client identity, and ALPN.
///
/// Binding, readiness, and shutdown are forwarded to the underlying connector.
/// The [`TlsConfig`] timeout starts after that connector returns, so it does not
/// bound DNS resolution or TCP connection establishment. Handshake errors are
/// returned as [`ConnectError`] with the TLS configuration's service identity.
pub struct TlsConnector<S> {
    /// Service responsible for producing the underlying, possibly filtered, I/O.
    svc: S,
    /// Shared TLS policy and session configuration.
    config: Arc<ClientConfig>,
}

impl<A: Address> TlsConnector<Connector<A>> {
    /// Creates a TLS connector using the default network connector.
    ///
    /// Use the `From<Arc<ClientConfig>>` implementation to share an existing
    /// rustls configuration instead of transferring an owned value.
    pub fn new(config: ClientConfig) -> Self {
        TlsConnector::from(Arc::new(config))
    }

    /// Replaces the default transport connector without changing the TLS policy.
    ///
    /// The replacement receives the original request and shared service state.
    /// Any filters on its returned I/O remain below TLS.
    pub fn connector<F, S>(self, f: impl IntoService<S, SharedCfg, Connect<A>>) -> TlsConnector<S>
    where
        S: Service<SharedCfg, Connect<A>, Res = Io<F>, Error = Error<ConnectError>>,
    {
        TlsConnector {
            svc: f.into_service(),
            config: self.config,
        }
    }
}

impl<A: Address> From<Arc<ClientConfig>> for TlsConnector<Connector<A>> {
    fn from(config: Arc<ClientConfig>) -> Self {
        TlsConnector {
            config,
            svc: Connector::default(),
        }
    }
}

impl<'a, A: Address> From<&'a Arc<ClientConfig>> for TlsConnector<Connector<A>> {
    fn from(config: &'a Arc<ClientConfig>) -> Self {
        TlsConnector {
            config: config.clone(),
            svc: Connector::default(),
        }
    }
}

impl<S: Clone> Clone for TlsConnector<S> {
    fn clone(&self) -> Self {
        Self {
            svc: self.svc.clone(),
            config: self.config.clone(),
        }
    }
}

impl<S: fmt::Debug> fmt::Debug for TlsConnector<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TlsConnector(rustls)")
            .field("svc", &self.svc)
            .finish()
    }
}

impl<F: Filter, A: Address, S> Service<SharedCfg, Connect<A>> for TlsConnector<S>
where
    S: Service<SharedCfg, Connect<A>, Res = Io<F>, Error = Error<ConnectError>>,
{
    type Res = Io<Layer<TlsClientFilter, F>>;
    type Error = Error<ConnectError>;

    /// Establishes transport first, then validates the server name and times the TLS handshake.
    async fn call(
        &self,
        req: Connect<A>,
        ctx: Ctx<'_, Self, SharedCfg>,
    ) -> Result<Self::Res, Self::Error> {
        let cfg = ctx.st().get::<TlsConfig>();
        let host = crate::utils::server_name(req.host()).to_owned();

        let io = ctx.call(&self.svc, req).await?;
        crate::utils::connect(&cfg, &host, async {
            let name = ServerName::try_from(host.as_str()).map_err(io::Error::other)?;
            TlsClientFilter::create(io, self.config.clone(), name.to_owned()).await
        })
        .await
    }

    ntex_service::forward!(SharedCfg, svc);
}

#[cfg(test)]
mod tests {
    use super::*;

    use ntex_service::Pipeline;
    use ntex_util::future::lazy;
    use tls_rustls::RootCertStore;

    /// STATEMENT: A rustls connector is ready but reports failure against a non-TLS peer.
    ///
    /// HOW:
    /// - Build and clone connectors from owned and shared configuration and check the debug name.
    /// - Check immediate readiness, then assert a call to a transport-dropping local service fails.
    #[ntex::test]
    async fn test_rustls_connect() {
        let server = ntex::server::test_server(async || {
            ntex::service::fn_service(async |_| Ok::<_, ()>(()))
        });

        let cert_store = webpki_roots::TLS_SERVER_ROOTS
            .iter()
            .cloned()
            .collect::<RootCertStore>();
        let config = ClientConfig::builder()
            .with_root_certificates(cert_store)
            .with_no_client_auth();
        let _: TlsConnector<Connector<&'static str>> = TlsConnector::new(config.clone()).clone();
        let svc = TlsConnector::from(Arc::new(config)).clone();
        assert!(format!("{svc:?}").contains("TlsConnector"), "{svc:?}");

        let srv = Pipeline::new(SharedCfg::default(), svc);
        // always ready
        assert!(lazy(|cx| srv.poll_ready(cx)).await.is_ready());
        let result = srv
            .call(Connect::new("").set_addr(Some(server.addr())))
            .await;
        assert!(result.is_err());
    }
}
