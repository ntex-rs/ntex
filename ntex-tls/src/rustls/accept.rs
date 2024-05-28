use std::{io, sync::Arc};

use tls_rust::ServerConfig;

use ntex_io::{Filter, Io, Layer};
use ntex_service::{Service, ServiceCtx, ServiceFactory};
use ntex_util::time::Millis;

use super::TlsServerFilter;
use crate::{counter::Counter, MAX_SSL_ACCEPT_COUNTER};

#[derive(Debug)]
/// Support `SSL` connections via rustls package
///
/// `rust-tls` feature enables `RustlsAcceptor` type
pub struct TlsAcceptor {
    config: Arc<ServerConfig>,
    timeout: Millis,
}

impl TlsAcceptor {
    /// Create rustls based `Acceptor` service factory
    pub fn new(config: Arc<ServerConfig>) -> Self {
        Self {
            config,
            timeout: Millis(5_000),
        }
    }

    /// Set handshake timeout.
    ///
    /// Default is set to 5 seconds.
    pub fn timeout<U: Into<Millis>>(mut self, timeout: U) -> Self {
        self.timeout = timeout.into();
        self
    }
}

impl From<ServerConfig> for TlsAcceptor {
    fn from(cfg: ServerConfig) -> Self {
        Self::new(Arc::new(cfg))
    }
}

impl Clone for TlsAcceptor {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            timeout: self.timeout,
        }
    }
}

impl<F: Filter, C> ServiceFactory<Io<F>, C> for TlsAcceptor {
    type Response = Io<Layer<TlsServerFilter, F>>;
    type Error = io::Error;
    type Service = TlsAcceptorService;
    type InitError = ();

    async fn create(&self, _: C) -> Result<Self::Service, Self::InitError> {
        MAX_SSL_ACCEPT_COUNTER.with(|conns| {
            Ok(TlsAcceptorService {
                config: self.config.clone(),
                timeout: self.timeout,
                conns: conns.clone(),
            })
        })
    }
}

#[derive(Debug)]
/// RusTLS based `Acceptor` service
pub struct TlsAcceptorService {
    config: Arc<ServerConfig>,
    timeout: Millis,
    conns: Counter,
}

impl<F: Filter> Service<Io<F>> for TlsAcceptorService {
    type Response = Io<Layer<TlsServerFilter, F>>;
    type Error = io::Error;

    async fn ready(&self, _: ServiceCtx<'_, Self>) -> Result<(), Self::Error> {
        self.conns.available().await;
        Ok(())
    }

    async fn call(
        &self,
        io: Io<F>,
        _: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        let _guard = self.conns.get();
        super::TlsServerFilter::create(io, self.config.clone(), self.timeout).await
    }
}
