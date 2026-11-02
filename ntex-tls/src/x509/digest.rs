//! Message digests used for certificate thumbprints.
//!
//! Windows uses `SymCrypt`. Elsewhere rustls uses standalone `RustCrypto` hash implementations
//! (independent of its TLS crypto provider); OpenSSL-only builds use OpenSSL.
//!
//! | Target  | Backend                 |
//! |---------|-------------------------|
//! | rustls, non-Windows | `sha1` / `sha2` |
//! | OpenSSL only, non-Windows | OpenSSL (`tls_openssl::sha`) |
//! | windows | `SymCrypt` (`symcrypt::hash`) |
//!
//! SHA-1 is provided only because the public API accepts 40 hex character
//! thumbprints (`allowedThumbprints`) which are SHA-1 digests of the DER
//! certificate. The public [`sha1()`] function is meant for identification only.
//! Supporting SHA-1 thumbprints does not enable SHA-1 certificate signatures; their
//! acceptance follows the trust backend's policy (see [`crate::x509::trust`]).

/// Size of a SHA-1 digest in bytes.
pub const SHA1_LEN: usize = 20;
/// Size of a SHA-256 digest in bytes.
pub const SHA256_LEN: usize = 32;

/// Computes the SHA-1 digest of `data`.
///
/// Identification only (40 hex character thumbprints), see the module docs.
#[must_use]
pub fn sha1(data: &[u8]) -> [u8; SHA1_LEN] {
    #[cfg(all(not(windows), not(feature = "rustls"), feature = "openssl"))]
    {
        tls_openssl::sha::sha1(data)
    }
    #[cfg(all(not(windows), feature = "rustls"))]
    {
        use sha1::Digest;
        sha1::Sha1::digest(data).into()
    }
    #[cfg(windows)]
    {
        symcrypt::hash::sha1(data)
    }
}

/// Computes the SHA-256 digest of `data`.
#[must_use]
pub fn sha256(data: &[u8]) -> [u8; SHA256_LEN] {
    #[cfg(all(not(windows), not(feature = "rustls"), feature = "openssl"))]
    {
        tls_openssl::sha::sha256(data)
    }
    #[cfg(all(not(windows), feature = "rustls"))]
    {
        use sha2::Digest;
        sha2::Sha256::digest(data).into()
    }
    #[cfg(windows)]
    {
        symcrypt::hash::sha256(data)
    }
}

/// Digest algorithm used for certificate thumbprints.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum Algorithm {
    /// SHA-1, 20 bytes / 40 hex characters. Identification only.
    Sha1,
    /// SHA-256, 32 bytes / 64 hex characters.
    Sha256,
}

impl Algorithm {
    /// Selects the algorithm for a hex encoded thumbprint.
    ///
    /// A thumbprint of exactly 40 characters is SHA-1, anything else is SHA-256
    /// (matches the behavior of the public `allowedThumbprints` API).
    #[must_use]
    pub const fn for_hex_thumbprint(thumbprint: &str) -> Self {
        if thumbprint.len() == SHA1_LEN * 2 {
            Algorithm::Sha1
        } else {
            Algorithm::Sha256
        }
    }

    /// Digest size in bytes.
    #[must_use]
    pub const fn digest_len(self) -> usize {
        match self {
            Algorithm::Sha1 => SHA1_LEN,
            Algorithm::Sha256 => SHA256_LEN,
        }
    }

    /// Computes the digest of `data`.
    #[must_use]
    pub fn digest(self, data: &[u8]) -> Vec<u8> {
        match self {
            Algorithm::Sha1 => sha1(data).to_vec(),
            Algorithm::Sha256 => sha256(data).to_vec(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// STATEMENT: Every selected thumbprint backend implements standard SHA-1 and SHA-256.
    ///
    /// HOW:
    /// - Compare the empty and `abc` message hashes against fixed NIST vectors.
    #[test]
    fn nist_vectors() {
        assert_eq!(
            hex::encode(sha1(b"")),
            "da39a3ee5e6b4b0d3255bfef95601890afd80709"
        );
        assert_eq!(
            hex::encode(sha1(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(
            hex::encode(sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex::encode(sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    /// STATEMENT: Thumbprint selection and digest dispatch use the documented lengths.
    ///
    /// HOW:
    /// - Select algorithms from hex lengths and compare dispatched hashes with direct calls.
    #[test]
    fn algorithm() {
        assert_eq!(
            Algorithm::for_hex_thumbprint(&"0".repeat(40)),
            Algorithm::Sha1
        );
        assert_eq!(
            Algorithm::for_hex_thumbprint(&"0".repeat(64)),
            Algorithm::Sha256
        );
        assert_eq!(Algorithm::for_hex_thumbprint("abc"), Algorithm::Sha256);
        assert_eq!(Algorithm::Sha1.digest_len(), 20);
        assert_eq!(Algorithm::Sha256.digest_len(), 32);
        assert_eq!(Algorithm::Sha1.digest(b"abc"), sha1(b"abc").to_vec());
        assert_eq!(Algorithm::Sha256.digest(b"abc"), sha256(b"abc").to_vec());
    }
}
