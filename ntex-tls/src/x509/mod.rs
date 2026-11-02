//! X.509 certificate handling: parsing, thumbprints, trust verification and identity loading.
//!
//! TLS acceptor/connector construction stays in the backend modules; certificate parsing,
//! thumbprints, trust verification and identity loading live here.
//!
//! # Backends
//!
//! Windows always uses native APIs. Elsewhere `rustls` takes precedence over `openssl`.
//! Both TLS transports remain available when both features are enabled. Verification
//! failures are never retried with another backend.
//!
//! | Configuration | Thumbprints | Trust | PKCS#12 |
//! |---------------|-------------|-------|---------|
//! | Windows | `SymCrypt` | Crypt32 | Native |
//! | rustls | SHA-1/SHA-256 crates | `WebPKI` | OpenSSL feature required on Unix |
//! | OpenSSL only | OpenSSL | OpenSSL (`PARTIAL_CHAIN`) | OpenSSL on Unix |
//!
//! Client identities ([`identity::ClientIdentity`]) keep the private key in CNG on Windows and
//! sign in place (`NCryptSignHash`).
//!
//! Certificate parsing uses `x509-parser` on all targets. Rustls-only builds do not depend
//! on OpenSSL. Install a rustls crypto provider before loading keys or verifying chains,
//! or enable `rustls-ring` to use ring when no provider is installed. Thumbprint hashing
//! does not require an installed provider.

pub mod auth;
pub mod cert;
#[cfg(windows)]
mod cng;
mod crypto;
pub mod digest;
mod error;
pub mod identity;
pub mod key;
pub mod peer;
pub mod trust;

pub use self::cert::{Certificate, KeyType, SanIp, SubjectAltNames, pem_encode};
pub use self::error::Error;
pub use self::identity::{ClientIdentity, Identity, IdentitySource};
pub use self::key::{HashAlg, PrivateKey, SignScheme};
pub use self::peer::PeerCerts;
pub use self::trust::{CaCerts, TrustError};

/// Installs a provider only in unit-test processes; production never installs one implicitly.
#[cfg(test)]
pub(crate) fn test_provider() {
    #[cfg(all(not(windows), feature = "rustls", not(feature = "rustls-ring")))]
    let _ = tls_rustls::crypto::ring::default_provider().install_default();
}
