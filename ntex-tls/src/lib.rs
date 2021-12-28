//! An implementations of SSL streams for ntex ecosystem
#![allow(clippy::return_self_not_must_use)]
use std::sync::atomic::{AtomicUsize, Ordering};

pub mod types;

#[cfg(feature = "openssl")]
pub mod openssl;

#[cfg(feature = "rustls")]
pub mod rustls;

mod counter;

/// Sets the maximum per-worker concurrent ssl connection establish process.
///
/// All listeners will stop accepting connections when this limit is
/// reached. It can be used to limit the global SSL CPU usage.
///
/// By default max connections is set to a 256.
pub fn max_concurrent_ssl_accept(num: usize) {
    MAX_SSL_ACCEPT.store(num, Ordering::Relaxed);
}

static MAX_SSL_ACCEPT: AtomicUsize = AtomicUsize::new(256);

thread_local! {
    static MAX_SSL_ACCEPT_COUNTER: counter::Counter = counter::Counter::new(MAX_SSL_ACCEPT.load(Ordering::Relaxed));
}
