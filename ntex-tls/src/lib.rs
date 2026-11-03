//! TLS services and I/O filters for the ntex ecosystem.
//!
//! Acceptor services upgrade existing [`ntex_io::Io`] connections on the server
//! side. Connector services open a transport and perform a client handshake.
//! Both preserve any filters already installed below TLS.
//!
//! # Backends
//!
//! No backend is enabled by default:
//!
//! - `openssl` exposes OpenSSL acceptors, connectors, and a bidirectional filter.
//! - `rustls` exposes rustls acceptors, connectors, and client/server filters.
//!   Supply a crypto provider through rustls configuration; `rustls-ring` enables
//!   its ring provider and also enables `rustls`.
//! - `schannel` is kept for compatibility; Windows Schannel support is always
//!   compiled on Windows (required by `x509`). Protocols, cipher suites, and
//!   trust defaults depend on the operating system.
//!
//! The `x509` module (certificate parsing, trust verification, identities) is
//! enabled by either TLS feature and always available on Windows. Windows uses
//! Crypt32/CNG; elsewhere rustls takes precedence over OpenSSL.
//! Explicitly enabling `x509` without a backend is an error.
//!
//! Certificate verification, identities, and ALPN are configured through the
//! selected backend. [`TlsConfig`] controls service handshake timeouts, while
//! [`max_concurrent_ssl_accept`] controls acceptor readiness backpressure.
//! Low-level filter constructors and handshake functions have their own timeout
//! contracts and do not participate in the acceptor concurrency limit.
//!
//! After negotiation, [`ntex_io::IoRef::query`] exposes connection metadata such
//! as [`PeerCertDer`], [`Servername`], [`TlsVersion`], and [`TlsCipher`].
//! Metadata availability depends on the backend and negotiated session.
//!
//! # Handshake configuration
//!
//! Attach this configuration to accepted I/O connections, or use it as the
//! shared state of a connector service:
//!
//! ```
//! use ntex_service::cfg::SharedCfg;
//! use ntex_tls::TlsConfig;
//! use ntex_util::time::Seconds;
//!
//! let config: SharedCfg = SharedCfg::new("TLS")
//!     .add(TlsConfig::new().set_handshake_timeout(Seconds(10)))
//!     .into();
//! assert_eq!(
//!     config.get::<TlsConfig>().handshake_timeout(),
//!     ntex_util::time::Millis(10_000),
//! );
//! ```
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

#[cfg(windows)]
pub mod schannel;

#[cfg(all(
    feature = "x509",
    not(any(windows, feature = "openssl", feature = "rustls"))
))]
compile_error!("ntex-tls x509 requires the openssl or rustls feature outside Windows");

#[cfg(any(windows, feature = "openssl", feature = "rustls"))]
pub mod x509;

use ntex_util::services::Counter;

/// Shared timeout configuration for TLS services.
mod config;
/// Backend-independent connection metadata.
mod types;
mod utils;

pub use self::config::TlsConfig;
pub use self::types::{
    PeerCertChainDer, PeerCertDer, PskIdentity, Servername, TlsCipher, TlsVersion,
};

/// Sets the per-worker soft limit on concurrent TLS handshakes.
///
/// TLS acceptors apply readiness backpressure when this limit is reached.
/// Readiness does not reserve a slot: independent listener or service pipelines
/// with cached readiness can still start handshakes and exceed the limit.
/// This controls handshake load but is not a strict admission limit.
///
/// The counter is shared by all TLS acceptors constructed on the same thread.
/// This call updates the current thread's counter and the initial limit for
/// threads whose counters have not yet been initialized. It does not update
/// counters already initialized on other threads; configure the limit before
/// starting workers, or update each existing worker separately.
///
/// The default is 256. A limit of zero keeps acceptor readiness pending;
/// it does not mean unlimited capacity. Direct handshake functions do not
/// acquire slots.
pub fn max_concurrent_ssl_accept(num: usize) {
    MAX_SSL_ACCEPT.store(num, Ordering::Relaxed);
    MAX_SSL_ACCEPT_COUNTER.with(|counts| counts.set_capacity(num));
}

/// Initial capacity read when a thread first creates its handshake counter.
static MAX_SSL_ACCEPT: AtomicUsize = AtomicUsize::new(256);

thread_local! {
    /// Handshake load shared by acceptors on this thread, independent of other workers.
    static MAX_SSL_ACCEPT_COUNTER: Counter = Counter::new(MAX_SSL_ACCEPT.load(Ordering::Relaxed));
}

/// Distinguishes a TLS failure from an application service failure.
#[derive(Debug)]
pub enum TlsError<E> {
    /// TLS setup, handshake, or transport failure.
    Tls(std::io::Error),
    /// Error returned by the wrapped application service.
    Service(E),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// STATEMENT: The configured handshake limit updates both the global limit and local capacity.
    ///
    /// HOW:
    /// - Set the limit to 256 and check the atomic value.
    /// - Hold 255 counter guards, then the final guard, and check availability at each step.
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
