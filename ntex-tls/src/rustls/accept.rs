use std::{io, marker::PhantomData, sync::Arc};

use tls_rustls::ServerConfig;

use ntex_io::{Filter, Io, Layer};
use ntex_service::{Ctx, ReadyCtx, Service, cfg::Cfg, cfg::Configuration};
use ntex_util::services::Counter;

use crate::{MAX_SSL_ACCEPT_COUNTER, TlsConfig, rustls::TlsServerFilter};

#[derive(Debug)]
/// `RusTLS` based `Acceptor` service
pub struct TlsAcceptor<F> {
    cfg: Arc<ServerConfig>,
    conns: Counter,
    st: PhantomData<F>,
}

impl<F> TlsAcceptor<F> {
    pub fn new(cfg: Arc<ServerConfig>) -> Self {
        MAX_SSL_ACCEPT_COUNTER.with(|conns| TlsAcceptor {
            cfg,
            conns: conns.clone(),
            st: PhantomData,
        })
    }
}

impl<F> From<ServerConfig> for TlsAcceptor<F> {
    fn from(cfg: ServerConfig) -> Self {
        Self::new(Arc::new(cfg))
    }
}

impl<F: Filter, St> Service<St> for TlsAcceptor<F> {
    type Req = Io<F>;
    type Res = Io<Layer<TlsServerFilter, F>>;
    type Error = io::Error;

    async fn ready(&self, _: ReadyCtx<'_, Self, St>) -> Result<(), Self::Error> {
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
