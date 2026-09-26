//! An implementations of SSL streams for ntex ecosystem
#![deny(clippy::pedantic)]
#![allow(
    clippy::clone_on_copy,
    clippy::missing_fields_in_debug,
    clippy::must_use_candidate,
    clippy::missing_errors_doc,
    clippy::unused_async_trait_impl
)]
use std::sync::atomic::{AtomicUsize, Ordering};

#[cfg(feature = "openssl")]
pub mod openssl;

#[cfg(feature = "rustls")]
pub mod rustls;

#[cfg(all(windows, feature = "schannel"))]
pub mod schannel;

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

#[derive(Debug)]
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

/// Ssl error combinded with service error.
#[derive(Debug)]
pub enum TlsError<E> {
    Tls(std::io::Error),
    Service(E),
}

/// Strips the port and IPv6 brackets from a connect host.
///
/// Accepts `host`, `host:port`, `[v6]`, `[v6]:port` and a bare `v6` address.
#[allow(dead_code)]
fn server_name(host: &str) -> &str {
    if let Some(rest) = host.strip_prefix('[') {
        rest.split_once(']').map_or(host, |(ip, _)| ip)
    } else {
        match host.split_once(':') {
            Some((name, port)) if !port.contains(':') => name,
            _ => host,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::server_name;

    #[test]
    fn test_server_name() {
        assert_eq!(server_name("example.com"), "example.com");
        assert_eq!(server_name("example.com:443"), "example.com");
        assert_eq!(server_name("127.0.0.1:8080"), "127.0.0.1");
        assert_eq!(server_name("[::1]"), "::1");
        assert_eq!(server_name("[::1]:443"), "::1");
        assert_eq!(server_name("[fe80::1%25eth0]:443"), "fe80::1%25eth0");
        assert_eq!(server_name("::1"), "::1");
        assert_eq!(server_name("2001:db8::1"), "2001:db8::1");
        assert_eq!(server_name(""), "");
    }
}
