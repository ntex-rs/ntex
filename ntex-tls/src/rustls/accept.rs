use std::{io, sync::Arc};

use tls_rustls::ServerConfig;

use ntex_io::{Filter, Io, Layer};
use ntex_service::cfg::{Cfg, SharedCfg};
use ntex_service::{Service, ServiceCtx};
use ntex_util::services::Counter;

use crate::{MAX_SSL_ACCEPT_COUNTER, TlsConfig, rustls::TlsServerFilter};

#[derive(Clone, Debug)]
/// Support `TLS` connections via rustls package
///
/// `rust-tls` feature enables `TlsAcceptor` type
pub struct TlsAcceptor {
    config: Arc<ServerConfig>,
}

impl TlsAcceptor {
    /// Create rustls based `Acceptor` service factory
    pub fn new(config: Arc<ServerConfig>) -> Self {
        Self { config }
    }
}

impl From<ServerConfig> for TlsAcceptor {
    fn from(cfg: ServerConfig) -> Self {
        Self::new(Arc::new(cfg))
    }
}

impl Service<SharedCfg> for TlsAcceptor {
    type Response = TlsAcceptorService;
    type Error = ();
    type Data = ();

    async fn call(
        &self,
        cfg: SharedCfg,
        _: &Self::Data,
        _: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        MAX_SSL_ACCEPT_COUNTER.with(|conns| {
            Ok(TlsAcceptorService {
                cfg: cfg.get(),
                config: self.config.clone(),
                conns: conns.clone(),
            })
        })
    }
}

#[derive(Debug)]
/// `RusTLS` based `Acceptor` service
pub struct TlsAcceptorService {
    cfg: Cfg<TlsConfig>,
    config: Arc<ServerConfig>,
    conns: Counter,
}

impl<F: Filter> Service<Io<F>> for TlsAcceptorService {
    type Response = Io<Layer<TlsServerFilter, F>>;
    type Error = io::Error;
    type Data = ();

    async fn ready(
        &self,
        _: &Self::Data,
        _: ServiceCtx<'_, Self>,
    ) -> Result<(), Self::Error> {
        if !self.conns.is_available() {
            self.conns.available().await;
        }
        Ok(())
    }

    async fn call(
        &self,
        io: Io<F>,
        _: &Self::Data,
        _: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        let _guard = self.conns.get();
        super::TlsServerFilter::create(
            io,
            self.config.clone(),
            self.cfg.handshake_timeout(),
        )
        .await
    }
}
