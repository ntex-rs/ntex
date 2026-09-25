use std::{fmt, io};

use ntex_io::{Filter, Io, Layer};
use ntex_service::cfg::Cfg;
use ntex_service::{Ctx, Service, cfg::Configuration};
use ntex_util::services::Counter;
use tls_openssl::ssl;

use crate::{MAX_SSL_ACCEPT_COUNTER, TlsConfig, openssl::SslFilter};

#[derive(Clone)]
/// Support `TLS` server connections via openssl package
///
/// `openssl` feature enables `Acceptor` type
pub struct SslAcceptor {
    acceptor: ssl::SslAcceptor,
    conns: Counter,
}

impl SslAcceptor {
    /// Create default openssl acceptor service
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

    async fn ready(&self, _: Ctx<'_, Self, St>) -> Result<(), Self::Error> {
        if !self.conns.is_available() {
            self.conns.available().await;
        }
        Ok(())
    }

    async fn call(&self, io: Io<F>, _: Ctx<'_, Self, St>) -> Result<Self::Res, Self::Error> {
        let _guard = self.conns.get();
        let ssl = ssl::Ssl::new(self.acceptor.context()).map_err(io::Error::other)?;
        let cfg: Cfg<TlsConfig> = io.cfg().ctx().get();

        log::trace!("{}: Accepting tls connection", io.tag());
        super::with_timeout(cfg.handshake_timeout(), super::handshake(io, ssl, true)).await
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
