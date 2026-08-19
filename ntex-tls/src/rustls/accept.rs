use std::{io, sync::Arc};

use tls_rustls::ServerConfig;

use ntex_io::{Filter, Io, Layer};
use ntex_service::{Ctx, Service, cfg::Cfg, cfg::Configuration};
use ntex_util::services::Counter;

use crate::{MAX_SSL_ACCEPT_COUNTER, TlsConfig, rustls::TlsServerFilter};

#[derive(Debug)]
/// `RusTLS` based `Acceptor` service
pub struct TlsAcceptor {
    cfg: Arc<ServerConfig>,
    conns: Counter,
}

impl TlsAcceptor {
    pub fn new(cfg: Arc<ServerConfig>) -> Self {
        MAX_SSL_ACCEPT_COUNTER.with(|conns| TlsAcceptor {
            cfg,
            conns: conns.clone(),
        })
    }
}

impl From<ServerConfig> for TlsAcceptor {
    fn from(cfg: ServerConfig) -> Self {
        Self::new(Arc::new(cfg))
    }
}

impl<F: Filter, St> Service<St, Io<F>> for TlsAcceptor {
    type Res = Io<Layer<TlsServerFilter, F>>;
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
        super::TlsServerFilter::create(io, self.cfg.clone(), cfg.handshake_timeout()).await
    }
}
