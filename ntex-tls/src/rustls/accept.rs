use std::{io, sync::Arc};

use tls_rust::ServerConfig;

use ntex_io::{Filter, Io, Layer};
use ntex_service::{Service, ServiceCtx, ServiceFactory};
use ntex_util::{services::Counter, time::Millis};

use crate::{rustls::TlsServerFilter, MAX_SSL_ACCEPT_COUNTER};

#[derive(Debug)]
/// Support `SSL` connections via rustls package
///
/// `rust-tls` feature enables `RustlsAcceptor` type
pub struct TlsAcceptor {
    config: Arc<ServerConfig>,
    timeout: Millis,
    tag: &'static str,
}

impl TlsAcceptor {
    /// Create rustls based `Acceptor` service factory
    pub fn new(config: Arc<ServerConfig>) -> Self {
        Self {
            config,
            timeout: Millis(5_000),
            tag: "TLS-ACCEPT",
        }
    }

    /// Set io tag
    ///
    /// Set tag to opened io object.
    pub fn tag(mut self, tag: &'static str) -> Self {
        self.tag = tag;
        self
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
            tag: self.tag,
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
                tag: self.tag,
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
    tag: &'static str,
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
        io.set_tag(self.tag);
        super::TlsServerFilter::create(io, self.config.clone(), self.timeout).await
    }
}
