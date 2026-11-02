//! Client certificate trust verification against a set of registered CA certificates.
//!
//! [`CaCerts::is_trusted`] semantics (all targets):
//! - an empty store never trusts anything;
//! - every registered certificate is a trust anchor, it does not have to be self-signed
//!   (OpenSSL `X509_V_FLAG_PARTIAL_CHAIN`); in particular a registered leaf certificate
//!   pins that leaf;
//! - certificates sent by the peer ([`PeerCerts::chain`]) are untrusted intermediates;
//! - validity periods of all certificates on the verified path, including the anchor, are
//!   checked against `now`;
//! - on success the earliest `not_after` of the verified path (leaf, intermediates and
//!   anchor) is returned, the session must not outlive it;
//! - no revocation checking.
//!
//! # Backends
//!
//! | Target  | Backend                                                         |
//! |---------|-----------------------------------------------------------------|
//! | rustls, non-Windows | `WebPKI` with the rustls crypto provider |
//! | OpenSSL only, non-Windows | OpenSSL `X509Store` with `PARTIAL_CHAIN`, default purpose checks |
//! | windows | Crypt32 private chain engine (exclusive roots)                  |
//!
//! The Crypt32 backend (Windows) differs from OpenSSL in these accepted ways:
//! - a certificate with an extended key usage extension that does not contain
//!   `clientAuth` is rejected (OpenSSL ignores EKU of intermediates);
//! - weak signature algorithms (e.g. SHA-1) follow the Windows weak crypto policy;
//! - basic constraints, X.509 v1 and critical extension handling follow Crypt32 rules;
//! - a pinned leaf (leaf DER equal to a registered certificate) is trusted after a validity
//!   check only, the chain engine does not support end-entity trust anchors;
//! - error strings in [`TrustError`] differ.
//!
//! `WebPKI` also enforces client-authentication EKU and its own encoding/algorithm policy.
//! It does not support verification times before 1970. SHA-1 thumbprints remain supported
//! even when the crypto provider rejects SHA-1 certificate signatures.
use std::{fmt, time::SystemTime};

use ntex_bytes::ByteString;

use crate::x509::{Certificate, PeerCerts, digest};

#[cfg(all(not(windows), not(feature = "rustls"), feature = "openssl"))]
#[path = "openssl.rs"]
mod backend;
#[cfg(all(not(windows), feature = "rustls"))]
#[path = "rustls.rs"]
mod backend;
#[cfg(windows)]
#[path = "crypt32.rs"]
mod backend;

/// Trust verification failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustError {
    /// Peer certificate is not trusted, or no CA certificates are registered.
    Unauthenticated(ByteString),
    /// Verification could not be performed.
    Internal(ByteString),
}

/// Registered CA certificates (trust anchors).
pub struct CaCerts {
    anchors: Vec<Certificate>,
    store: backend::Store,
}

impl CaCerts {
    /// Builds a trust store from certificates.
    ///
    /// Certificates the backend refuses are logged (with the SHA-1 thumbprint) and skipped.
    #[must_use]
    pub fn from_certs<I, C>(certs: I) -> Self
    where
        I: IntoIterator<Item = C>,
        C: Into<Certificate>,
    {
        Self::from_certs_with(certs.into_iter().map(|c| {
            let c = c.into();
            let sha1 = c.thumbprint(digest::Algorithm::Sha1);
            (c, move |e: &str| {
                log::error!("Unable to add cert '{sha1}' (SHA1 thumbprint) into X509Store: {e}");
            })
        }))
    }

    /// Builds a trust store from certificates, each paired with an error handler.
    ///
    /// The handler is called with the backend error if the certificate is refused; the
    /// certificate is then skipped. Duplicates are added once and their handler is not called.
    /// Lets callers report refusals with their own context (e.g. a configured name).
    pub fn from_certs_with<I, F>(certs: I) -> Self
    where
        I: IntoIterator<Item = (Certificate, F)>,
        F: FnOnce(&str),
    {
        let mut builder = backend::Builder::new();
        let mut anchors: Vec<Certificate> = Vec::new();
        for (cert, on_error) in certs {
            if anchors.contains(&cert) {
                continue;
            }
            match builder.add(&cert) {
                Ok(()) => anchors.push(cert),
                Err(e) => on_error(&e),
            }
        }
        CaCerts {
            anchors,
            store: builder.build(),
        }
    }

    /// Number of registered certificates (duplicates are counted once).
    #[must_use]
    pub fn len(&self) -> usize {
        self.anchors.len()
    }

    /// Returns `true` if no certificates are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.anchors.is_empty()
    }

    /// Registered certificates.
    #[must_use]
    pub fn certs(&self) -> &[Certificate] {
        &self.anchors
    }

    /// Verifies the peer certificates against the registered CA certificates at time `now`.
    ///
    /// Returns the earliest `not_after` of the verified path, see the module docs.
    ///
    /// # Errors
    /// - [`TrustError::Unauthenticated`] if no CA certificates are registered or the peer
    ///   certificate is rejected;
    /// - [`TrustError::Internal`] if the verification could not be performed.
    pub fn is_trusted(&self, peer: &PeerCerts, now: SystemTime) -> Result<SystemTime, TrustError> {
        if self.is_empty() {
            return Err(TrustError::Unauthenticated(
                "No CA certificates registered".into(),
            ));
        }
        self.store.verify(peer, now)
    }
}

impl Default for CaCerts {
    fn default() -> Self {
        Self::from_certs_with(std::iter::empty::<(Certificate, fn(&str))>())
    }
}

impl fmt::Debug for CaCerts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("CaCerts").field(&self.anchors).finish()
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;
    use crate::x509::testgen::{self, San};

    /// Fixed-date fixtures (P-256, SHA-256) so the tests run on every backend:
    /// - `root` 2020..2120 -> `inter` (CA) 2021..2110 -> `leaf` 2022..2100;
    /// - `other` 2020..2120, unrelated self-signed CA;
    /// - `shortroot` 2020..2060 -> `shortleaf` 2022..2100.
    fn c(name: &str) -> Certificate {
        crate::x509::test_provider();
        let pem: &[u8] = match name {
            "root" => include_bytes!("../../../tests/testdata/root.pem"),
            "inter" => include_bytes!("../../../tests/testdata/inter.pem"),
            "leaf" => include_bytes!("../../../tests/testdata/leaf.pem"),
            "other" => include_bytes!("../../../tests/testdata/other.pem"),
            "shortroot" => include_bytes!("../../../tests/testdata/shortroot.pem"),
            "shortleaf" => include_bytes!("../../../tests/testdata/shortleaf.pem"),
            _ => unreachable!(),
        };
        Certificate::from_pem(pem).unwrap()
    }

    /// January 1st of `year`, 00:00 UTC.
    fn year(year: u64) -> SystemTime {
        let days: u64 = (1970..year)
            .map(|y| {
                if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
                    366
                } else {
                    365
                }
            })
            .sum();
        UNIX_EPOCH + Duration::from_secs(days * 86400)
    }

    const NOW: u64 = 2050;

    /// STATEMENT: Empty stores reject every peer.
    ///
    /// HOW:
    /// - Verify the fixed leaf/intermediate against an empty store and check its error.
    #[test]
    fn empty_store() {
        let peer = PeerCerts::new(c("leaf"), vec![c("inter")]);
        let store = CaCerts::default();
        assert!(store.is_empty());
        assert_eq!(
            store.is_trusted(&peer, year(NOW)),
            Err(TrustError::Unauthenticated(
                "No CA certificates registered".into()
            ))
        );
    }

    /// STATEMENT: A root trusts its complete chain but not missing or unrelated issuers.
    ///
    /// HOW:
    /// - Register duplicate roots and verify the fixed chain, then remove or replace its issuer.
    #[test]
    fn chain_to_root() {
        let store = CaCerts::from_certs([c("root"), c("root")]);
        assert_eq!(store.len(), 1);

        let peer = PeerCerts::new(c("leaf"), vec![c("inter")]);
        assert_eq!(
            store.is_trusted(&peer, year(NOW)).unwrap(),
            c("leaf").not_after()
        );

        // missing intermediate
        let peer = PeerCerts::new(c("leaf"), vec![]);
        assert!(matches!(
            store.is_trusted(&peer, year(NOW)),
            Err(TrustError::Unauthenticated(_))
        ));

        // untrusted root
        let store = CaCerts::from_certs([c("other")]);
        let peer = PeerCerts::new(c("leaf"), vec![c("inter")]);
        assert!(matches!(
            store.is_trusted(&peer, year(NOW)),
            Err(TrustError::Unauthenticated(_))
        ));
    }

    /// STATEMENT: Intermediates and exact leaves can be registered as anchors.
    ///
    /// HOW:
    /// - Verify a leaf against its intermediate and itself; reject its expired pin.
    #[test]
    fn partial_chain() {
        // intermediate as anchor, expiry is min(leaf, inter)
        let store = CaCerts::from_certs([c("inter")]);
        let peer = PeerCerts::new(c("leaf"), vec![]);
        assert_eq!(
            store.is_trusted(&peer, year(NOW)).unwrap(),
            c("leaf").not_after()
        );

        // pinned leaf
        let store = CaCerts::from_certs([c("leaf")]);
        assert_eq!(
            store.is_trusted(&peer, year(NOW)).unwrap(),
            c("leaf").not_after()
        );
        assert!(matches!(
            store.is_trusted(&peer, year(2101)),
            Err(TrustError::Unauthenticated(_))
        ));
    }

    /// STATEMENT: Returned expiry includes the selected trust anchor.
    ///
    /// HOW:
    /// - Verify a leaf whose root expires first and assert the root's expiry is returned.
    #[test]
    fn expiry_is_min_of_path() {
        let store = CaCerts::from_certs([c("shortroot")]);
        let peer = PeerCerts::new(c("shortleaf"), vec![]);
        assert_eq!(c("shortroot").not_after(), year(2060));
        assert_eq!(store.is_trusted(&peer, year(NOW)).unwrap(), year(2060));
    }

    /// STATEMENT: Verification uses the supplied time instead of the current clock.
    ///
    /// HOW:
    /// - Verify the fixed chain before, within and after its validity interval.
    #[test]
    fn uses_now() {
        let store = CaCerts::from_certs([c("root")]);
        let peer = PeerCerts::new(c("leaf"), vec![c("inter")]);
        assert!(store.is_trusted(&peer, year(2101)).is_err());
        assert!(store.is_trusted(&peer, year(2021)).is_err());
        assert!(store.is_trusted(&peer, year(2099)).is_ok());
    }

    /// STATEMENT: Unused certificates cannot shorten a verified path's lifetime.
    ///
    /// HOW:
    /// - Add an unrelated shorter-lived root to both anchors and peer intermediates.
    /// - Check the real chain still expires with its leaf.
    #[test]
    fn unrelated_expiry() {
        let store = CaCerts::from_certs([c("shortroot"), c("root")]);
        let peer = PeerCerts::new(c("leaf"), vec![c("shortroot"), c("inter"), c("other")]);
        assert_eq!(
            store.is_trusted(&peer, year(NOW)).unwrap(),
            c("leaf").not_after()
        );
    }

    /// STATEMENT: An expired anchor cannot authenticate an otherwise valid leaf.
    ///
    /// HOW:
    /// - Verify the short-root fixture after the root expires but before its leaf expires.
    #[test]
    fn expired_anchor() {
        let store = CaCerts::from_certs([c("shortroot")]);
        let peer = PeerCerts::new(c("shortleaf"), vec![]);
        assert!(matches!(
            store.is_trusted(&peer, year(2070)),
            Err(TrustError::Unauthenticated(_))
        ));
    }

    /// STATEMENT: `WebPKI` reports unsupported historical verification times explicitly.
    ///
    /// HOW:
    /// - Attempt verification one second before the Unix epoch and check the internal error.
    #[cfg(all(not(windows), feature = "rustls"))]
    #[test]
    fn before_unix_epoch() {
        let store = CaCerts::from_certs([c("root")]);
        let peer = PeerCerts::new(c("leaf"), vec![c("inter")]);
        assert!(matches!(
            store.is_trusted(&peer, UNIX_EPOCH - Duration::from_secs(1)),
            Err(TrustError::Internal(_))
        ));
    }

    /// STATEMENT: Trust expires with an intermediate even when the leaf and root outlive it.
    ///
    /// HOW:
    /// - Generate a chain whose intermediate expires before its leaf and root.
    /// - Require the intermediate's expiry now and reject the chain a day after it.
    #[test]
    fn intermediate_expiry() {
        let san = San::default();
        let (rk, ik) = (testgen::key(), testgen::key());
        let root = testgen::cert("root", true, None, &rk, &san, (-1, 3000));
        let inter = testgen::cert("inter", true, Some((&root, &rk)), &ik, &san, (-1, 1000));
        let leaf = testgen::cert(
            "leaf",
            false,
            Some((&inter, &ik)),
            &testgen::key(),
            &san,
            (-1, 2000),
        );
        let store = CaCerts::from_certs([root]);
        let peer = PeerCerts::new(leaf, vec![inter.clone()]);
        let now = SystemTime::now();
        assert_eq!(store.is_trusted(&peer, now).unwrap(), inter.not_after());
        let later = inter.not_after() + Duration::from_hours(24);
        assert!(later < peer.leaf().not_after());
        assert!(matches!(
            store.is_trusted(&peer, later),
            Err(TrustError::Unauthenticated(_))
        ));
    }

    /// STATEMENT: CA trust enforces client-auth EKU while exact leaf pinning remains explicit.
    ///
    /// HOW:
    /// - Issue leaves without EKU, with client-auth, with both, and server-auth only.
    /// - Reject only the server-auth leaf under the CA, but permit each exact leaf pin.
    /// - Reject a client-auth leaf issued by a server-auth-only intermediate.
    #[cfg(any(windows, feature = "rustls"))]
    #[test]
    fn client_auth_usage() {
        use crate::x509::testgen::ExtKeyUsage::{ClientAuth, ServerAuth};
        let san = San::default();
        let now = SystemTime::now();
        let ck = testgen::key();
        let ca = testgen::cert("ca", true, None, &ck, &san, (-1, 10));
        let store = CaCerts::from_certs([ca.clone()]);
        let issue = |issuer, ca, eku| {
            testgen::cert_with_eku("x", ca, Some(issuer), &testgen::key(), &san, (-1, 10), eku)
        };
        for (eku, accepted) in [
            (&[][..], true),
            (&[ClientAuth][..], true),
            (&[ServerAuth, ClientAuth][..], true),
            (&[ServerAuth][..], false),
        ] {
            let peer = PeerCerts::new(issue((&ca, &ck), false, eku), vec![]);
            assert_eq!(store.is_trusted(&peer, now).is_ok(), accepted, "{eku:?}");
            let pinned = CaCerts::from_certs([peer.leaf().clone()]);
            assert_eq!(
                pinned.is_trusted(&peer, now).unwrap(),
                peer.leaf().not_after()
            );
        }

        let ik = testgen::key();
        let inter = testgen::cert_with_eku(
            "inter",
            true,
            Some((&ca, &ck)),
            &ik,
            &san,
            (-1, 10),
            &[ServerAuth],
        );
        let peer = PeerCerts::new(issue((&inter, &ik), false, &[ClientAuth]), vec![inter]);
        assert!(matches!(
            store.is_trusted(&peer, now),
            Err(TrustError::Unauthenticated(_))
        ));
    }

    /// STATEMENT: Rejected anchors invoke their callback and do not displace valid deduplicated anchors.
    ///
    /// HOW:
    /// - Supply a CA with duplicate basic constraints between two copies of a valid root.
    /// - Check its callback fires once and the remaining store still authenticates the fixture chain.
    #[cfg(all(not(windows), feature = "rustls"))]
    #[test]
    fn rejected_anchor_callback() {
        use rcgen::{BasicConstraints, CertificateParams, CustomExtension, IsCa, KeyPair};
        use std::cell::RefCell;
        let mut bad = CertificateParams::default();
        bad.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        bad.custom_extensions = vec![CustomExtension::from_oid_content(
            &[2, 5, 29, 19],
            vec![0x30, 0],
        )];
        let bad = bad.self_signed(&KeyPair::generate().unwrap()).unwrap();
        let bad = Certificate::from_der(bad.der().to_vec()).unwrap();
        let rejected = RefCell::new(Vec::new());
        let store =
            CaCerts::from_certs_with([c("root"), bad, c("root")].into_iter().enumerate().map(
                |(index, cert)| {
                    let rejected = &rejected;
                    (cert, move |error: &str| {
                        rejected.borrow_mut().push((index, error.to_owned()));
                    })
                },
            ));
        assert_eq!(rejected.borrow().len(), 1);
        assert_eq!(rejected.borrow()[0].0, 1);
        assert_ne!(rejected.borrow()[0].1, "");
        assert_eq!(store.certs(), &[c("root")]);
        let peer = PeerCerts::new(c("leaf"), vec![c("inter")]);
        assert_eq!(
            store.is_trusted(&peer, year(NOW)).unwrap(),
            peer.leaf().not_after()
        );
    }
}
