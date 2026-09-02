use std::io;

use ntex_error::Error;
use ntex_io::{Filter, Io, Layer};
use ntex_net::connect::{Address, Connect, ConnectError, Connector};
use ntex_service::{Ctx, IntoService, Service, cfg::SharedCfg};
use ntex_util::time::timeout_checked;

use super::{ClientConfig, SchannelFilter, connect as connect_io};
use crate::TlsConfig;

#[derive(Clone, Debug)]
pub struct TlsConnector<Sf> {
    svc: Sf,
    config: ClientConfig,
}

impl<A: Address> Default for TlsConnector<Connector<A>> {
    fn default() -> Self {
        Self::new()
    }
}

impl<A: Address> TlsConnector<Connector<A>> {
    /// Construct new Schannel connector factory.
    pub fn new() -> Self {
        TlsConnector {
            svc: Connector::default(),
            config: ClientConfig::default(),
        }
    }

    /// Construct new Schannel connector factory with custom configuration.
    pub fn with_config(config: ClientConfig) -> Self {
        TlsConnector {
            svc: Connector::default(),
            config,
        }
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

impl<S> TlsConnector<S> {
    /// Establish a TLS connection on top of an existing I/O stream.
    pub async fn connect<F: Filter>(
        &self,
        io: Io<F>,
        host: &str,
        cfg: &SharedCfg,
    ) -> Result<Io<Layer<SchannelFilter, F>>, Error<ConnectError>> {
        let cfg = cfg.get::<TlsConfig>();
        log::trace!("{}: TLS Handshake start for: {host:?} {io:?}", cfg.tag());

        async {
            match timeout_checked(
                cfg.handshake_timeout(),
                connect_io(io, host, self.config.clone()),
            )
            .await
            {
                Ok(Ok(io)) => {
                    log::trace!("{}: TLS Handshake success: {host:?}", cfg.tag());
                    Ok(io)
                }
                Ok(Err(e)) => {
                    log::trace!("{}: TLS Handshake error: {e:?}", cfg.tag());
                    Err(ConnectError::from(e).into())
                }
                Err(()) => {
                    log::trace!("{}: TLS Handshake timeout", cfg.tag());
                    Err(ConnectError::from(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "TLS Handshake timeout",
                    ))
                    .into())
                }
            }
        }
        .await
        .map_err(|e: Error<_>| e.set_service(cfg.service()))
    }
}

impl<F: Filter, A: Address, S> Service<SharedCfg, Connect<A>> for TlsConnector<S>
where
    S: Service<SharedCfg, Connect<A>, Res = Io<F>, Error = Error<ConnectError>>,
{
    type Res = Io<Layer<SchannelFilter, F>>;
    type Error = Error<ConnectError>;

    async fn call(
        &self,
        req: Connect<A>,
        ctx: Ctx<'_, Self, SharedCfg>,
    ) -> Result<Self::Res, Self::Error> {
        let host = req.host().split(':').next().unwrap().to_string();
        let io = ctx.call(&self.svc, req).await?;
        self.connect(io, &host, ctx.st()).await
    }

    ntex_service::forward_ready!(SharedCfg, svc);
    ntex_service::forward_shutdown!(SharedCfg, svc);
}

#[cfg(test)]
mod tests {
    use ntex_service::Pipeline;

    use super::*;

    #[ntex::test]
    async fn test_schannel_connect() {
        let server = ntex::server::test_server(async || {
            ntex::service::fn_service(|_| async { Ok::<_, ()>(()) })
        });

        let svc: TlsConnector<Connector<&'static str>> = TlsConnector::new();
        assert!(format!("{svc:?}").contains("TlsConnector"));
        let srv = Pipeline::with(SharedCfg::default(), svc);
        assert!(srv.ready().await.is_ok());
        let result = srv
            .call(Connect::new("").set_addr(Some(server.addr())))
            .await;
        assert!(result.is_err());
    }
}
