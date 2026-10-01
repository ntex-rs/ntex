use std::io;

use ntex_error::Error;
use ntex_io::{Io, Layer};
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
        Self::with_config(ClientConfig::default())
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

impl<A: Address, S> Service<SharedCfg, Connect<A>> for TlsConnector<S>
where
    S: Service<SharedCfg, Connect<A>, Res = Io, Error = Error<ConnectError>>,
{
    type Res = Io<Layer<SchannelFilter>>;
    type Error = Error<ConnectError>;

    ntex_service::forward_ready!(SharedCfg, svc);
    ntex_service::forward_shutdown!(SharedCfg, svc);

    async fn call(
        &self,
        message: Connect<A>,
        ctx: Ctx<'_, Self, SharedCfg>,
    ) -> Result<Self::Res, Self::Error> {
        let cfg = ctx.st().get::<TlsConfig>();
        let host = crate::server_name(message.host()).to_string();

        let io = ctx.call(&self.svc, message).await?;
        let tag = io.tag();
        log::trace!("{tag}: TLS Handshake start for: {host:?}");

        let res = timeout_checked(
            cfg.handshake_timeout(),
            connect_io(io, &host, self.config.clone()),
        )
        .await
        .unwrap_or_else(|()| {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "TLS Handshake timeout",
            ))
        });
        match res {
            Ok(io) => {
                log::trace!("{tag}: TLS Handshake success: {host:?}");
                Ok(io)
            }
            Err(e) => {
                log::trace!("{tag}: TLS Handshake error: {e:?}");
                Err(ConnectError::from(e).into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use ntex_io::{testing::IoTest, types::HttpProtocol};
    use ntex_service::{Pipeline, fn_service};
    use ntex_util::{future::join, time::Millis};

    use super::*;
    use crate::schannel::{Certificate, ServerConfig, accept};

    const PFX: &[u8] = include_bytes!("../../examples/identity.pfx");

    /// Connector over an in-memory transport.
    fn io_connector(
        connector: TlsConnector<Connector<&'static str>>,
        io: Io,
    ) -> TlsConnector<
        impl Service<SharedCfg, Connect<&'static str>, Res = Io, Error = Error<ConnectError>>,
    > {
        let io = Rc::new(RefCell::new(Some(io)));
        connector.connector(fn_service(move |_: Connect<&'static str>| {
            let io = io.borrow_mut().take().unwrap();
            async move { Ok::<_, Error<ConnectError>>(io) }
        }))
    }

    #[ntex::test]
    async fn test_schannel_connect() {
        let server = ntex::server::test_server(async || {
            ntex::service::fn_service(|_| async { Ok::<_, ()>(()) })
        });

        let svc: TlsConnector<Connector<&'static str>> = TlsConnector::new();
        assert!(format!("{svc:?}").contains("TlsConnector"));
        let srv = Pipeline::new(SharedCfg::default(), svc);
        assert!(srv.ready().await.is_ok());
        let result = srv
            .call(Connect::new("").set_addr(Some(server.addr())))
            .await;
        assert!(result.is_err());
    }

    #[ntex::test]
    async fn test_schannel_connector() {
        let config = ServerConfig::new(Certificate::from_pkcs12(PFX, "ntex").unwrap())
            .unwrap()
            .set_alpn_protocols(&[b"h2"]);
        let client = ClientConfig::new()
            .danger_accept_invalid_certs(true)
            .set_alpn_protocols(&[b"h2"]);

        let (cli, srv) = IoTest::create();
        cli.remote_buffer_cap(1 << 20);
        srv.remote_buffer_cap(1 << 20);
        let connector = io_connector(
            TlsConnector::with_config(client),
            Io::new(cli, SharedCfg::new("CLI")),
        );
        let connector = Pipeline::new(SharedCfg::new("CLI").build(), connector);
        let (client, server) = join(
            connector.call(Connect::new("localhost")),
            accept(Io::new(srv, SharedCfg::new("SRV")), &config),
        )
        .await;
        let client = client.unwrap();
        server.unwrap();
        assert_eq!(
            client.query::<HttpProtocol>().as_ref(),
            Some(&HttpProtocol::Http2)
        );
    }

    /// The default configuration verifies the server certificate.
    #[ntex::test]
    async fn test_schannel_connector_untrusted() {
        let config = ServerConfig::new(Certificate::from_pkcs12(PFX, "ntex").unwrap()).unwrap();

        let (cli, srv) = IoTest::create();
        cli.remote_buffer_cap(1 << 20);
        srv.remote_buffer_cap(1 << 20);
        let connector = io_connector(TlsConnector::default(), Io::new(cli, SharedCfg::new("CLI")));
        let connector = Pipeline::new(SharedCfg::new("CLI").build(), connector);
        let (client, server) = join(
            connector.call(Connect::new("localhost")),
            accept(Io::new(srv, SharedCfg::new("SRV")), &config),
        )
        .await;
        let err = client.unwrap_err();
        assert!(matches!(&*err, ConnectError::Io(_)), "{err:?}");
        assert!(server.is_err());
    }

    #[ntex::test]
    async fn test_schannel_connector_timeout() {
        let tls_cfg = TlsConfig {
            handshake_timeout: Millis(50),
            ..TlsConfig::default()
        };
        let (cli, _srv) = IoTest::create();
        let connector = io_connector(
            TlsConnector::with_config(ClientConfig::new()),
            Io::new(cli, SharedCfg::new("CLI")),
        );
        let connector = Pipeline::new(SharedCfg::new("CLI").add(tls_cfg).build(), connector);
        let err = connector.call(Connect::new("localhost")).await.unwrap_err();
        let ConnectError::Io(err) = &*err else {
            panic!("{err:?}");
        };
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }
}
