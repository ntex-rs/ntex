use std::{io, sync::Arc};

use tls_rust::ServerConfig;

use ntex_io::{Cfg, Filter, Io, Layer, SharedConfig};
use ntex_service::{Service, ServiceCtx, ServiceFactory};
use ntex_util::services::Counter;

use crate::{MAX_SSL_ACCEPT_COUNTER, TlsConfig, rustls::TlsServerFilter};

#[derive(Clone, Debug)]
/// Support `SSL` connections via rustls package
///
/// `rust-tls` feature enables `RustlsAcceptor` type
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

impl<F: Filter> ServiceFactory<Io<F>, SharedConfig> for TlsAcceptor {
    type Response = Io<Layer<TlsServerFilter, F>>;
    type Error = io::Error;
    type Service = TlsAcceptorService;
    type InitError = ();

    async fn create(&self, cfg: SharedConfig) -> Result<Self::Service, Self::InitError> {
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
/// RusTLS based `Acceptor` service
pub struct TlsAcceptorService {
    cfg: Cfg<TlsConfig>,
    config: Arc<ServerConfig>,
    conns: Counter,
}

impl<F: Filter> Service<Io<F>> for TlsAcceptorService {
    type Response = Io<Layer<TlsServerFilter, F>>;
    type Error = io::Error;

    async fn ready(&self, _: ServiceCtx<'_, Self>) -> Result<(), Self::Error> {
        if !self.conns.is_available() {
            self.conns.available().await
        }
        Ok(())
    }

    async fn call(
        &self,
        io: Io<F>,
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
