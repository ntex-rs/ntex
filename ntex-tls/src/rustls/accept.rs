//! Server handshake service with shared per-thread capacity and an I/O-configured timeout.
use std::{io, sync::Arc};

use ntex_io::{Filter, Io, Layer};
use ntex_service::{BindCtx, Ctx, Service, cfg::Cfg, cfg::Configuration};
use ntex_util::services::Counter;
use tls_rustls::ServerConfig;

use crate::{MAX_SSL_ACCEPT_COUNTER, TlsConfig, rustls::TlsServerFilter};

#[derive(Debug)]
/// Upgrades existing I/O connections to server-side TLS using rustls.
///
/// Reads [`TlsConfig`] from each connection's configuration. Readiness observes
/// the shared per-thread soft limit set by [`crate::max_concurrent_ssl_accept`];
/// it does not reserve a handshake slot. Calling the service holds a slot until
/// the handshake completes, fails, times out, or is cancelled.
///
/// Existing I/O filters remain below the returned [`TlsServerFilter`].
pub struct TlsAcceptor {
    /// Shared certificate, protocol, and client-authentication policy.
    cfg: Arc<ServerConfig>,
    /// Thread-local handshake counter also used by other backend acceptors.
    conns: Counter,
}

impl TlsAcceptor {
    /// Creates an acceptor using the supplied rustls server configuration.
    ///
    /// Captures the current thread's handshake counter; the timeout is resolved
    /// separately for each accepted connection.
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

    /// Registers capacity-change notifications with the service readiness scope.
    fn bind(&self, ctx: BindCtx<'_>) {
        self.conns.bind(&ctx);
    }

    /// Waits for spare capacity without reserving it.
    async fn ready(&self, _: Ctx<'_, Self, St>) -> Result<(), Self::Error> {
        if !self.conns.is_available() {
            self.conns.available().await;
        }
        Ok(())
    }

    /// Counts this handshake and applies the timeout from the connection's registry.
    async fn call(&self, io: Io<F>, _: Ctx<'_, Self, St>) -> Result<Self::Res, Self::Error> {
        let _guard = self.conns.get();
        let cfg: Cfg<TlsConfig> = io.cfg().ctx().get();
        super::TlsServerFilter::create(io, self.cfg.clone(), cfg.handshake_timeout()).await
    }
}
