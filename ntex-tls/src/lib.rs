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

use ntex_util::services::Counter;

mod config;
mod types;
mod utils;

pub use self::config::TlsConfig;
pub use self::types::{PeerCertChainDer, PeerCertDer, PskIdentity, Servername};

/// Sets the per-worker soft limit on concurrent TLS handshakes.
///
/// TLS acceptors apply readiness backpressure when this limit is reached.
/// Readiness does not reserve a slot: independent listener or service pipelines
/// with cached readiness can still start handshakes and exceed the limit.
/// This controls handshake load but is not a strict admission limit.
///
/// By default the limit is 256.
pub fn max_concurrent_ssl_accept(num: usize) {
    MAX_SSL_ACCEPT.store(num, Ordering::Relaxed);
    MAX_SSL_ACCEPT_COUNTER.with(|counts| counts.set_capacity(num));
}

static MAX_SSL_ACCEPT: AtomicUsize = AtomicUsize::new(256);

thread_local! {
    static MAX_SSL_ACCEPT_COUNTER: Counter = Counter::new(MAX_SSL_ACCEPT.load(Ordering::Relaxed));
}

/// Ssl error combinded with service error.
#[derive(Debug)]
pub enum TlsError<E> {
    Tls(std::io::Error),
    Service(E),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_concurrent_accept() {
        // the default value, other tests use the limit
        max_concurrent_ssl_accept(256);
        assert_eq!(MAX_SSL_ACCEPT.load(Ordering::Relaxed), 256);
        MAX_SSL_ACCEPT_COUNTER.with(|c| {
            let _guards: Vec<_> = (0..255).map(|_| c.get()).collect();
            assert!(c.is_available());
            let _last = c.get();
            assert!(!c.is_available());
        });
    }
}
