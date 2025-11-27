use std::io;

use ntex_io::{Io, Layer};
use ntex_net::connect::{Address, Connect, ConnectError, Connector};
use ntex_service::cfg::{Cfg, SharedCfg};
use ntex_service::{Service, ServiceCtx, ServiceFactory};
use ntex_util::time::timeout_checked;
use tls_openssl::ssl::SslConnector as OpensslConnector;

use super::{SslFilter, connect as connect_io};
use crate::TlsConfig;

#[derive(Clone, Debug)]
pub struct SslConnector<S> {
    connector: S,
    openssl: OpensslConnector,
}

#[derive(Clone, Debug)]
pub struct SslConnectorService<S> {
    svc: S,
    cfg: Cfg<TlsConfig>,
    openssl: OpensslConnector,
}

impl<A: Address> SslConnector<Connector<A>> {
    /// Construct new OpensslConnectService factory
    pub fn new(connector: OpensslConnector) -> Self {
        SslConnector {
            openssl: connector,
            connector: Connector::default(),
        }
    }
}

impl<A: Address, S> ServiceFactory<Connect<A>, SharedCfg> for SslConnector<S>
where
    S: ServiceFactory<Connect<A>, SharedCfg, Response = Io, Error = ConnectError>,
{
    type Response = Io<Layer<SslFilter>>;
    type Error = ConnectError;
    type Service = SslConnectorService<S::Service>;
    type InitError = S::InitError;

    async fn create(&self, cfg: SharedCfg) -> Result<Self::Service, Self::InitError> {
        let svc = self.connector.create(cfg).await?;

        Ok(SslConnectorService {
            svc,
            cfg: cfg.get(),
            openssl: self.openssl.clone(),
        })
    }
}

impl<A: Address, S> Service<Connect<A>> for SslConnectorService<S>
where
    S: Service<Connect<A>, Response = Io, Error = ConnectError>,
{
    type Response = Io<Layer<SslFilter>>;
    type Error = ConnectError;

    ntex_service::forward_ready!(svc);
    ntex_service::forward_poll!(svc);
    ntex_service::forward_shutdown!(svc);

    async fn call(
        &self,
        message: Connect<A>,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        let host = message.host().split(':').next().unwrap().to_string();

        let io = ctx.call(&self.svc, message).await?;
        let tag = io.tag();
        log::trace!("{tag}: SSL Handshake start for: {host:?}");

        match self.openssl.configure() {
            Err(e) => Err(io::Error::new(io::ErrorKind::InvalidInput, e).into()),
            Ok(config) => {
                let ssl = config
                    .into_ssl(&host)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

                match timeout_checked(self.cfg.handshake_timeout(), connect_io(io, ssl))
                    .await
                {
                    Ok(Ok(io)) => {
                        log::trace!("{tag}: SSL Handshake success: {host:?}");
                        Ok(io)
                    }
                    Ok(Err(e)) => {
                        log::trace!("{tag}: SSL Handshake error: {e:?}");
                        Err(e.into())
                    }
                    Err(_) => {
                        log::trace!("{tag}: SSL Handshake timeout");
                        Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "SSL Handshake timeout",
                        )
                        .into())
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tls_openssl::ssl::SslMethod;

    #[ntex::test]
    async fn test_openssl_connect() {
        let server = ntex::server::test_server(|| {
            ntex::service::fn_service(|_| async { Ok::<_, ()>(()) })
        });

        let ssl = OpensslConnector::builder(SslMethod::tls()).unwrap();
        let _: SslConnector<Connector<&'static str>> = SslConnector::new(ssl.build());
        let ssl = OpensslConnector::builder(SslMethod::tls()).unwrap();
        let factory = SslConnector::new(ssl.build()).clone();

        let srv = factory.pipeline(SharedCfg::default()).await.unwrap();
        // always ready
        assert!(srv.ready().await.is_ok());
        let result = srv
            .call(Connect::new("").set_addr(Some(server.addr())))
            .await;
        assert!(result.is_err());
        assert!(format!("{srv:?}").contains("SslConnector"));
    }
}
