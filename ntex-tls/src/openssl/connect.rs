use std::io;

use ntex_error::Error;
use ntex_io::{Filter, Io, Layer};
use ntex_net::connect::{Address, Connect, ConnectError, Connector};
use ntex_service::{Ctx, IntoService, Service, cfg::SharedCfg};
use tls_openssl::ssl::SslConnector as OpensslConnector;

use crate::{TlsConfig, openssl::SslFilter};

#[derive(Clone, Debug)]
pub struct SslConnector<S> {
    svc: S,
    openssl: OpensslConnector,
}

impl<A: Address> SslConnector<Connector<A>> {
    /// Construct new `SslConnector` factory
    pub fn new(openssl: OpensslConnector) -> Self {
        SslConnector {
            openssl,
            svc: Connector::default(),
        }
    }

    /// Use connector to open connections.
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
    /// Establish a TLS connection on top of an existing I/O stream.
    pub async fn connect<F: Filter>(
        &self,
        io: Io<F>,
        host: &str,
        cfg: &SharedCfg,
    ) -> Result<Io<Layer<SslFilter, F>>, Error<ConnectError>> {
        let cfg = cfg.get::<TlsConfig>();
        log::trace!("{}: SSL Handshake start for: {host:?} {io:?}", cfg.tag());

        let result = async {
            let ssl = self
                .openssl
                .configure()
                .and_then(|config| config.into_ssl(host))
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
            super::with_timeout(cfg.handshake_timeout(), super::handshake(io, ssl, false)).await
        }
        .await;

        match result {
            Ok(io) => {
                log::trace!("{}: SSL Handshake success: {host:?}", cfg.tag());
                Ok(io)
            }
            Err(e) => {
                log::trace!("{}: SSL Handshake error: {e:?}", cfg.tag());
                Err(Error::from(ConnectError::from(e)).set_service(cfg.service()))
            }
        }
    }
}

impl<F: Filter, A: Address, S> Service<SharedCfg, Connect<A>> for SslConnector<S>
where
    S: Service<SharedCfg, Connect<A>, Res = Io<F>, Error = Error<ConnectError>>,
{
    type Res = Io<Layer<SslFilter, F>>;
    type Error = Error<ConnectError>;

    async fn call(
        &self,
        req: Connect<A>,
        ctx: Ctx<'_, Self, SharedCfg>,
    ) -> Result<Self::Res, Self::Error> {
        let host = crate::server_name(req.host()).to_string();
        let io = ctx.call(&self.svc, req).await?;
        self.connect(io, &host, ctx.st()).await
    }

    ntex_service::forward_ready!(SharedCfg, svc);
    ntex_service::forward_shutdown!(SharedCfg, svc);
}

#[cfg(test)]
mod tests {
    use super::*;

    use ntex_service::Pipeline;
    use tls_openssl::ssl::SslMethod;

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
