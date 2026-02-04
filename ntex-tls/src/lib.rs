//! An implementations of SSL streams for ntex ecosystem
#![deny(clippy::pedantic)]
#![allow(
    clippy::missing_fields_in_debug,
    clippy::must_use_candidate,
    clippy::missing_errors_doc,
    clippy::struct_field_names
)]

use std::sync::atomic::{AtomicUsize, Ordering};

#[cfg(feature = "openssl")]
pub mod openssl;

#[cfg(feature = "rustls")]
pub mod rustls;

use ntex_service::cfg::{CfgContext, Configuration};
use ntex_util::{services::Counter, time::Millis, time::Seconds};

/// Sets the maximum per-worker concurrent ssl connection establish process.
///
/// All listeners will stop accepting connections when this limit is
/// reached. It can be used to limit the global SSL CPU usage.
///
/// By default max connections is set to a 256.
pub fn max_concurrent_ssl_accept(num: usize) {
    MAX_SSL_ACCEPT.store(num, Ordering::Relaxed);
    MAX_SSL_ACCEPT_COUNTER.with(|counts| counts.set_capacity(num));
}

static MAX_SSL_ACCEPT: AtomicUsize = AtomicUsize::new(256);

thread_local! {
    static MAX_SSL_ACCEPT_COUNTER: Counter = Counter::new(MAX_SSL_ACCEPT.load(Ordering::Relaxed));
}

/// A TLS PSK identity.
///
/// Used in conjunction with [`ntex_io::Filter::query`]:
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PskIdentity(pub Vec<u8>);

/// The TLS SNI server name (DNS).
///
/// Used in conjunction with [`ntex_io::Filter::query`]:
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Servername(pub String);

#[derive(Debug, Clone)]
/// Tls service configuration
pub struct TlsConfig {
    handshake_timeout: Millis,
    config: CfgContext,
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
    #[allow(clippy::new_without_default)]
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
