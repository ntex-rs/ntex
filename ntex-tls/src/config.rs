use ntex_service::cfg::{CfgContext, Configuration};
use ntex_util::time::{Millis, Seconds};

#[derive(Debug)]
/// Handshake timeout shared by TLS acceptor and connector services.
///
/// Acceptors obtain this value from the accepted connection's I/O configuration;
/// connectors obtain it from their [`ntex_service::cfg::SharedCfg`] service state.
/// The timeout covers TLS negotiation, not the underlying TCP connection attempt
/// or the lifetime of an established connection.
///
/// This does not configure certificates, verification, ALPN, or protocol versions.
/// Use the selected backend's configuration for those settings.
pub struct TlsConfig {
    /// Handshake deadline duration; zero disables the deadline.
    pub(crate) handshake_timeout: Millis,
    /// Configuration registry context used for service identity and diagnostics.
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
    /// Creates a configuration with a five-second handshake timeout.
    pub fn new() -> Self {
        TlsConfig {
            handshake_timeout: Millis(5_000),
            config: CfgContext::default(),
        }
    }

    #[inline]
    /// Returns the handshake timeout; zero means no handshake deadline.
    pub fn handshake_timeout(&self) -> Millis {
        self.handshake_timeout
    }

    #[must_use]
    /// Sets the handshake timeout using a duration convertible to seconds.
    ///
    /// Use [`Seconds::ZERO`] to disable the deadline. Expiration is reported as
    /// [`std::io::ErrorKind::TimedOut`], wrapped in a connector error when applicable.
    ///
    /// The default is five seconds. Low-level handshake functions do not consult
    /// this configuration unless their documentation explicitly says otherwise.
    pub fn set_handshake_timeout<T: Into<Seconds>>(mut self, timeout: T) -> Self {
        self.handshake_timeout = timeout.into().into();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// STATEMENT: The default handshake timeout can be overridden or disabled.
    ///
    /// HOW:
    /// - Check the default five-second timeout, change it to one second, then set it to zero.
    /// - Compare the reported timeout in milliseconds after each change.
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
