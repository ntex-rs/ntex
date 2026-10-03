use ntex_service::cfg::{CfgContext, Configuration};
use ntex_util::time::{Millis, Seconds};

#[derive(Debug)]
/// Tls service configuration
pub struct TlsConfig {
    pub(crate) handshake_timeout: Millis,
    pub(crate) config: CfgContext,
}

impl Default for TlsConfig {
    fn default() -> Self {
        TlsConfig::new()
    }
}

impl Configuration for TlsConfig {
    const NAME: &str = "TLS Configuration";

    fn ctx(&self) -> &CfgContext {
        &self.config
    }

    fn set_ctx(&mut self, ctx: CfgContext) {
        self.config = ctx;
    }
}

impl TlsConfig {
    #[must_use]
    /// Create instance of `TlsConfig`
    pub fn new() -> Self {
        TlsConfig {
            handshake_timeout: Millis(5_000),
            config: CfgContext::default(),
        }
    }

    #[inline]
    /// Get tls handshake timeout.
    pub fn handshake_timeout(&self) -> Millis {
        self.handshake_timeout
    }

    #[must_use]
    /// Set tls handshake timeout.
    ///
    /// Defines a timeout for connection tls handshake negotiation.
    /// To disable timeout set value to 0.
    ///
    /// By default handshake timeout is set to 5 seconds.
    pub fn set_handshake_timeout<T: Into<Seconds>>(mut self, timeout: T) -> Self {
        self.handshake_timeout = timeout.into().into();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tls_config() {
        let cfg = TlsConfig::default();
        assert_eq!(cfg.handshake_timeout(), Millis(5_000));
        let cfg = cfg.set_handshake_timeout(Seconds(1));
        assert_eq!(cfg.handshake_timeout(), Millis(1_000));
        assert_eq!(
            cfg.set_handshake_timeout(Seconds::ZERO).handshake_timeout(),
            Millis::ZERO
        );
    }
}
