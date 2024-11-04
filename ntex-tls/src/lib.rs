//! An implementations of SSL streams for ntex ecosystem
#![deny(rust_2018_idioms, unreachable_pub, missing_debug_implementations)]

use std::sync::atomic::{AtomicUsize, Ordering};

#[cfg(feature = "openssl")]
pub mod openssl;

#[cfg(feature = "rustls")]
pub mod rustls;

use ntex_util::services::Counter;

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
