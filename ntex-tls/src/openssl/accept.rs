//! Server handshake service with shared per-thread capacity and an I/O-configured timeout.
use std::{fmt, io};

use ntex_io::{Filter, Io, Layer};
use ntex_service::cfg::Cfg;
use ntex_service::{BindCtx, Ctx, Service, cfg::Configuration};
use ntex_util::services::Counter;
use tls_openssl::ssl;

use crate::{MAX_SSL_ACCEPT_COUNTER, TlsConfig, openssl::SslFilter};

#[derive(Clone)]
/// Upgrades existing I/O connections to server-side TLS using OpenSSL.
///
/// Reads [`TlsConfig`] from each connection's configuration. Readiness observes
/// the shared per-thread soft limit set by [`crate::max_concurrent_ssl_accept`]
/// without reserving a slot. Each call holds a slot until completion, error,
/// timeout, or cancellation. Clones share both the OpenSSL context and counter.
///
/// Existing filters remain below the returned [`SslFilter`].
pub struct SslAcceptor {
    /// Certificate and TLS policy used to create each connection's session.
    acceptor: ssl::SslAcceptor,
    /// Thread-local handshake counter shared with other TLS acceptors.
    conns: Counter,
}

impl SslAcceptor {
    /// Creates a service from a prepared OpenSSL acceptor.
    ///
    /// Captures the current thread's handshake counter without changing the
    /// supplied certificate, verification, or protocol settings.
    pub fn new(acceptor: ssl::SslAcceptor) -> Self {
        MAX_SSL_ACCEPT_COUNTER.with(|conns| SslAcceptor {
            acceptor,
            conns: conns.clone(),
        })
    }
}

impl<F: Filter, St> Service<St, Io<F>> for SslAcceptor {
    type Res = Io<Layer<SslFilter, F>>;
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

    /// Counts the handshake, creates its session, and applies the I/O-configured timeout.
    async fn call(&self, io: Io<F>, _: Ctx<'_, Self, St>) -> Result<Self::Res, Self::Error> {
        let _guard = self.conns.get();
        let ssl = ssl::Ssl::new(self.acceptor.context()).map_err(io::Error::other)?;
        let cfg: Cfg<TlsConfig> = io.cfg().ctx().get();

        log::trace!("{}: Accepting tls connection", io.tag());
        crate::utils::with_timeout(cfg.handshake_timeout(), super::accept(io, ssl)).await
    }
}

impl From<ssl::SslAcceptor> for SslAcceptor {
    fn from(acceptor: ssl::SslAcceptor) -> Self {
        Self::new(acceptor)
    }
}

impl fmt::Debug for SslAcceptor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SslAcceptor").finish()
    }
}
