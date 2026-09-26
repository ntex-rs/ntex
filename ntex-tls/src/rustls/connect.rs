use std::{fmt, io, sync::Arc};

use ntex_error::Error;
use ntex_io::{Filter, Io, Layer};
use ntex_net::connect::{Address, Connect, ConnectError, Connector};
use ntex_service::{Ctx, IntoService, Service, cfg::SharedCfg};
use tls_rustls::{ClientConfig, pki_types::ServerName};

use crate::{TlsConfig, rustls::TlsClientFilter};

/// Rustls connector factory
pub struct TlsConnector<S> {
    svc: S,
    config: Arc<ClientConfig>,
}

impl<A: Address> TlsConnector<Connector<A>> {
    pub fn new(config: ClientConfig) -> Self {
        TlsConnector::from(Arc::new(config))
    }

    /// Use connector to open connections.
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

    async fn call(
        &self,
        req: Connect<A>,
        ctx: Ctx<'_, Self, SharedCfg>,
    ) -> Result<Self::Res, Self::Error> {
        let cfg = ctx.st().get::<TlsConfig>();
        let host = crate::server_name(req.host()).to_owned();

        let io = ctx.call(&self.svc, req).await?;
        log::trace!("{}: TLS Handshake start for: {host:?}", cfg.tag());

        let result = async {
            let name = ServerName::try_from(host.as_str()).map_err(io::Error::other)?;
            let fut = TlsClientFilter::create(io, self.config.clone(), name.to_owned());
            super::with_timeout(cfg.handshake_timeout(), fut).await
        }
        .await;

        match result {
            Ok(io) => {
                log::trace!("{}: TLS Handshake success: {host:?}", cfg.tag());
                Ok(io)
            }
            Err(e) => {
                log::trace!("{}: TLS Handshake error: {e:?}", cfg.tag());
                Err(Error::from(ConnectError::from(e)).set_service(cfg.service()))
            }
        }
    }

    ntex_service::forward_ready!(SharedCfg, svc);
    ntex_service::forward_shutdown!(SharedCfg, svc);
}

#[cfg(test)]
mod tests {
    use super::*;

    use ntex_service::Pipeline;
    use ntex_util::future::lazy;
    use tls_rustls::RootCertStore;

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
