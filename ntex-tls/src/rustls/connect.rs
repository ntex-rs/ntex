use std::{fmt, io, sync::Arc};

use ntex_error::Error;
use ntex_io::{Io, Layer};
use ntex_net::connect::{Address, Connect, ConnectError, Connector, Connector2};
use ntex_service::{Service, ServiceCtx, cfg::Cfg, cfg::SharedCfg};
use ntex_util::time::timeout_checked;
use tls_rustls::{ClientConfig, pki_types::ServerName};

use crate::{TlsConfig, rustls::TlsClientFilter};

/// Rustls connector factory
pub struct TlsConnector<S> {
    connector: S,
    config: Arc<ClientConfig>,
}

#[derive(Clone, Debug)]
pub struct TlsConnectorService<S> {
    svc: S,
    cfg: Cfg<TlsConfig>,
    config: Arc<ClientConfig>,
}

impl<A: Address> From<Arc<ClientConfig>> for TlsConnector<Connector<A>> {
    fn from(config: Arc<ClientConfig>) -> Self {
        TlsConnector {
            config,
            connector: Connector::default(),
        }
    }
}

impl<'a, A: Address> From<&'a Arc<ClientConfig>> for TlsConnector<Connector<A>> {
    fn from(config: &'a Arc<ClientConfig>) -> Self {
        TlsConnector {
            config: config.clone(),
            connector: Connector::default(),
        }
    }
}

impl<A: Address> TlsConnector<Connector<A>> {
    pub fn new(config: ClientConfig) -> Self {
        TlsConnector::from(Arc::new(config))
    }
}

impl<S: Clone> Clone for TlsConnector<S> {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            connector: self.connector.clone(),
        }
    }
}

impl<S: fmt::Debug> fmt::Debug for TlsConnector<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TlsConnector(rustls)")
            .field("connector", &self.connector)
            .finish()
    }
}

impl<S> Service<SharedCfg> for TlsConnector<S>
where
    S: Service<SharedCfg>,
{
    type Response = TlsConnectorService<S::Response>;
    type Error = S::Error;
    type Data = S::Data;

    async fn call(
        &self,
        cfg: SharedCfg,
        data: &Self::Data,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        let tls_cfg = cfg.get();
        let svc = ctx.call(&self.connector, cfg, data).await?;

        Ok(TlsConnectorService {
            svc,
            cfg: tls_cfg,
            config: self.config.clone(),
        })
    }
}

impl<A: Address, S> Service<Connect<A>> for TlsConnectorService<S>
where
    S: Service<Connect<A>, Response = Io, Error = ConnectError>,
{
    type Response = Io<Layer<TlsClientFilter>>;
    type Error = ConnectError;
    type Data = S::Data;

    ntex_service::forward_ready!(svc);
    ntex_service::forward_poll!(svc);
    ntex_service::forward_shutdown!(svc);

    async fn call(
        &self,
        req: Connect<A>,
        data: &Self::Data,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        let host = req.host().split(':').next().unwrap().to_owned();

        let io = ctx.call(&self.svc, req, data).await?;
        let tag = io.tag();
        log::trace!("{tag}: TLS Handshake start for: {host:?}");

        let config = self.config.clone();
        let host = ServerName::try_from(host).map_err(io::Error::other)?;

        let connect_fut = TlsClientFilter::create(io, config, host.clone());
        match timeout_checked(self.cfg.handshake_timeout(), connect_fut).await {
            Ok(Ok(io)) => {
                log::trace!("{tag}: TLS Handshake success: {host:?}");
                Ok(io)
            }
            Ok(Err(e)) => {
                log::trace!("{tag}: TLS Handshake error: {e:?}");
                Err(e.into())
            }
            Err(()) => {
                log::trace!("{tag}: TLS Handshake timeout");
                Err(io::Error::new(io::ErrorKind::TimedOut, "SSL Handshake timeout").into())
            }
        }
    }
}

/// Rustls connector factory
pub struct TlsConnector2<S> {
    connector: S,
    config: Arc<ClientConfig>,
}

#[derive(Clone, Debug)]
pub struct TlsConnectorService2<S> {
    svc: S,
    cfg: Cfg<TlsConfig>,
    config: Arc<ClientConfig>,
}

impl<A: Address> From<Arc<ClientConfig>> for TlsConnector2<Connector2<A>> {
    fn from(config: Arc<ClientConfig>) -> Self {
        TlsConnector2 {
            config,
            connector: Connector2::default(),
        }
    }
}

impl<'a, A: Address> From<&'a Arc<ClientConfig>> for TlsConnector2<Connector2<A>> {
    fn from(config: &'a Arc<ClientConfig>) -> Self {
        TlsConnector2 {
            config: config.clone(),
            connector: Connector2::default(),
        }
    }
}

impl<A: Address> TlsConnector2<Connector2<A>> {
    pub fn new(config: ClientConfig) -> Self {
        TlsConnector2::from(Arc::new(config))
    }
}

impl<S: Clone> Clone for TlsConnector2<S> {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            connector: self.connector.clone(),
        }
    }
}

impl<S: fmt::Debug> fmt::Debug for TlsConnector2<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TlsConnector(rustls)")
            .field("connector", &self.connector)
            .finish()
    }
}

impl<S> Service<SharedCfg> for TlsConnector2<S>
where
    S: Service<SharedCfg>,
{
    type Response = TlsConnectorService2<S::Response>;
    type Error = S::Error;
    type Data = S::Data;

    async fn call(
        &self,
        cfg: SharedCfg,
        data: &Self::Data,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        let tls_cfg = cfg.get();
        let svc = ctx.call(&self.connector, cfg, data).await?;

        Ok(TlsConnectorService2 {
            svc,
            cfg: tls_cfg,
            config: self.config.clone(),
        })
    }
}

impl<A: Address, S> Service<Connect<A>> for TlsConnectorService2<S>
where
    S: Service<Connect<A>, Response = Io, Error = Error<ConnectError>>,
{
    type Response = Io<Layer<TlsClientFilter>>;
    type Error = Error<ConnectError>;
    type Data = S::Data;

    ntex_service::forward_ready!(svc);
    ntex_service::forward_poll!(svc);
    ntex_service::forward_shutdown!(svc);

    async fn call(
        &self,
        req: Connect<A>,
        data: &Self::Data,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        let host = req.host().split(':').next().unwrap().to_owned();

        let io = ctx.call(&self.svc, req, data).await?;
        let tag = io.tag();
        log::trace!("{tag}: TLS Handshake start for: {host:?}");

        let config = self.config.clone();

        async {
            let host = ServerName::try_from(host)
                .map_err(|e| ConnectError::from(io::Error::other(e)))?;

            let connect_fut = TlsClientFilter::create(io, config, host.clone());
            match timeout_checked(self.cfg.handshake_timeout(), connect_fut).await {
                Ok(Ok(io)) => {
                    log::trace!("{tag}: TLS Handshake success: {host:?}");
                    Ok(io)
                }
                Ok(Err(e)) => {
                    log::trace!("{tag}: TLS Handshake error: {e:?}");
                    Err(ConnectError::from(e).into())
                }
                Err(()) => {
                    log::trace!("{tag}: TLS Handshake timeout");
                    Err(ConnectError::from(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "SSL Handshake timeout",
                    ))
                    .into())
                }
            }
        }
        .await
        .map_err(|e: Error<_>| e.set_service(self.cfg.service()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use ntex_service::ServiceFactory;
    use ntex_util::future::lazy;
    use tls_rustls::RootCertStore;

    #[ntex::test]
    async fn test_rustls_connect() {
        let server = ntex::server::test_server(async || {
            ntex::service::fn_service(|_| async { Ok::<_, ()>(()) })
        });

        let cert_store = webpki_roots::TLS_SERVER_ROOTS
            .iter()
            .cloned()
            .collect::<RootCertStore>();
        let config = ClientConfig::builder()
            .with_root_certificates(cert_store)
            .with_no_client_auth();
        let _: TlsConnector<Connector<&'static str>> =
            TlsConnector::new(config.clone()).clone();
        let factory = TlsConnector::from(Arc::new(config)).clone();
        assert!(
            format!("{factory:?}").contains("TlsConnector"),
            "{factory:?}"
        );

        let srv = factory
            .pipeline(SharedCfg::default(), &())
            .await
            .unwrap()
            .bind();
        // always ready
        assert!(lazy(|cx| srv.poll_ready(cx)).await.is_ready());
        let result = srv
            .call(Connect::new("").set_addr(Some(server.addr())))
            .await;
        assert!(result.is_err());
        assert!(format!("{srv:?}").contains("TlsConnectorService"));
    }

    #[ntex::test]
    async fn test_rustls_connect2() {
        let server = ntex::server::test_server(async || {
            ntex::service::fn_service(|_| async { Ok::<_, ()>(()) })
        });

        let cert_store = webpki_roots::TLS_SERVER_ROOTS
            .iter()
            .cloned()
            .collect::<RootCertStore>();
        let config = ClientConfig::builder()
            .with_root_certificates(cert_store)
            .with_no_client_auth();
        let _: TlsConnector2<Connector2<&'static str>> =
            TlsConnector2::new(config.clone()).clone();
        let factory = TlsConnector2::from(Arc::new(config)).clone();
        assert!(
            format!("{factory:?}").contains("TlsConnector"),
            "{factory:?}"
        );

        let srv = factory
            .pipeline(SharedCfg::default(), &())
            .await
            .unwrap()
            .bind();
        // always ready
        assert!(lazy(|cx| srv.poll_ready(cx)).await.is_ready());
        let result = srv
            .call(Connect::new("").set_addr(Some(server.addr())))
            .await;
        assert!(result.is_err());
        assert!(format!("{srv:?}").contains("TlsConnectorService2"));
    }
}
