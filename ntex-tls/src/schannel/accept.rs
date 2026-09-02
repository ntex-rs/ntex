use std::io;

use ntex_io::{Filter, Io, Layer};
use ntex_service::{Ctx, Service, cfg::Cfg, cfg::Configuration};
use ntex_util::{services::Counter, time};

use super::{SchannelFilter, ServerConfig, accept as accept_io};
use crate::{MAX_SSL_ACCEPT_COUNTER, TlsConfig};

/// Support TLS server connections via Windows Schannel.
#[derive(Clone, Debug)]
pub struct TlsAcceptor {
    config: ServerConfig,
    conns: Counter,
}

impl TlsAcceptor {
    /// Create a Schannel acceptor service.
    #[must_use]
    pub fn new(config: ServerConfig) -> Self {
        MAX_SSL_ACCEPT_COUNTER.with(|conns| TlsAcceptor {
            config,
            conns: conns.clone(),
        })
    }
}

impl From<ServerConfig> for TlsAcceptor {
    fn from(config: ServerConfig) -> Self {
        Self::new(config)
    }
}

impl<F: Filter, St> Service<St, Io<F>> for TlsAcceptor {
    type Res = Io<Layer<SchannelFilter, F>>;
    type Error = io::Error;

    async fn ready(&self, _: Ctx<'_, Self, St>) -> Result<(), Self::Error> {
        if !self.conns.is_available() {
            self.conns.available().await;
        }
        Ok(())
    }

    async fn call(&self, io: Io<F>, _: Ctx<'_, Self, St>) -> Result<Self::Res, Self::Error> {
        let _guard = self.conns.get();
        let cfg: Cfg<TlsConfig> = io.cfg().ctx().get();
        time::timeout(cfg.handshake_timeout(), accept_io(io, self.config.clone()))
            .await
            .map_err(|()| io::Error::new(io::ErrorKind::TimedOut, "TLS Handshake timeout"))
            .and_then(|item| item)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[ntex::test]
    async fn test_schannel_accept() {
        let config = ServerConfig::from_pem(
            include_str!("../../examples/cert.pem"),
            include_str!("../../examples/key.pem"),
        )
        .unwrap();
        let srv = TlsAcceptor::new(config.clone());
        assert!(format!("{srv:?}").contains("TlsAcceptor"));
        assert!(!config.cert_der().is_empty());
    }
}
