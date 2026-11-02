//! Process-isolated coverage for rustls provider configuration and backend precedence.
#![cfg(all(feature = "rustls", not(windows)))]

use ntex_tls::x509::{CaCerts, Certificate, Error, Identity, PeerCerts, TrustError};

/// STATEMENT: Only the explicit ring feature permits fallback, and installed providers take priority.
///
/// HOW:
/// - Hash without a provider; key loading and trust require the explicit ring fallback feature.
/// - Install a ring-based provider with no certificate-verification algorithms.
/// - Load its matching key but reject a valid chain, without falling back to another backend.
#[test]
fn configured_provider() {
    let root =
        Certificate::from_pem(include_bytes!("../src/x509/trust/testdata/root.pem")).unwrap();
    let inter =
        Certificate::from_pem(include_bytes!("../src/x509/trust/testdata/inter.pem")).unwrap();
    let leaf =
        Certificate::from_pem(include_bytes!("../src/x509/trust/testdata/leaf.pem")).unwrap();
    let now = leaf.not_before() + std::time::Duration::from_secs(60);
    let store = CaCerts::from_certs([root]);
    let peer = PeerCerts::new(leaf, vec![inter]);
    let cert = include_bytes!("../examples/cert.pem");
    let key = include_bytes!("../examples/key.pem");
    assert!(tls_rustls::crypto::CryptoProvider::get_default().is_none());
    assert_eq!(peer.leaf().sha1().len(), 20);
    assert_eq!(peer.leaf().sha256().len(), 32);
    if cfg!(feature = "rustls-ring") {
        assert!(Identity::from_pem(cert, key).is_ok());
        assert!(store.is_trusted(&peer, now).is_ok());
    } else {
        assert!(matches!(
            Identity::from_pem(cert, key),
            Err(Error::Backend(_))
        ));
        assert!(matches!(
            store.is_trusted(&peer, now),
            Err(TrustError::Internal(_))
        ));
    }

    let mut provider = tls_rustls::crypto::ring::default_provider();
    provider.signature_verification_algorithms.all = &[];
    provider.install_default().unwrap();
    assert!(Identity::from_pem(cert, key).is_ok());
    assert!(matches!(
        store.is_trusted(&peer, now),
        Err(TrustError::Unauthenticated(_))
    ));
}
