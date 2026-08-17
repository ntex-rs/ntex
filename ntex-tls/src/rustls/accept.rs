use std::{io, marker::PhantomData, sync::Arc};

use tls_rustls::ServerConfig;

use ntex_io::{Filter, Io, Layer};
use ntex_service::cfg::{Cfg, SharedCfg};
use ntex_service::{Ctx, ReadyCtx, Service, ServiceFactory};
use ntex_util::services::Counter;

use crate::{MAX_SSL_ACCEPT_COUNTER, TlsConfig, rustls::TlsServerFilter};

#[derive(Clone, Debug)]
/// Support `TLS` connections via rustls package
///
/// `rust-tls` feature enables `TlsAcceptor` type
pub struct TlsAcceptorFactory {
    config: Arc<ServerConfig>,
}

impl TlsAcceptorFactory {
    /// Create rustls based `Acceptor` service factory
    pub fn new(config: Arc<ServerConfig>) -> Self {
        Self { config }
    }
}

impl From<ServerConfig> for TlsAcceptorFactory {
    fn from(cfg: ServerConfig) -> Self {
        Self::new(Arc::new(cfg))
    }
}

impl<F: Filter, St> ServiceFactory<Io<F>, St> for TlsAcceptorFactory {
    type Res = Io<Layer<TlsServerFilter, F>>;
    type Error = io::Error;
    type Service = TlsAcceptor<F>;
    type InitCfg = SharedCfg;
    type InitError = ();

    async fn create(&self, cfg: &SharedCfg) -> Result<Self::Service, Self::InitError> {
        MAX_SSL_ACCEPT_COUNTER.with(|conns| {
            Ok(TlsAcceptor {
                cfg: cfg.get(),
                config: self.config.clone(),
                conns: conns.clone(),
                st: PhantomData,
            })
        })
    }
}

#[derive(Debug)]
/// `RusTLS` based `Acceptor` service
pub struct TlsAcceptor<F> {
    cfg: Cfg<TlsConfig>,
    config: Arc<ServerConfig>,
    conns: Counter,
    st: PhantomData<F>,
}

impl<F> TlsAcceptor<F> {
    pub fn new(config: Arc<ServerConfig>) -> Self {
        MAX_SSL_ACCEPT_COUNTER.with(|conns| TlsAcceptor {
            config,
            cfg: Cfg::default(),
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
        super::TlsServerFilter::create(io, self.config.clone(), self.cfg.handshake_timeout()).await
    }
}
