//! Portable private-key ownership and signing.
//!
//! Exportable PKCS#8 material is zeroized when its final owner drops. Windows keys remain
//! in CNG, including keys imported from PEM/DER, and cannot be exported through this API.
use std::fmt;
#[cfg(not(windows))]
use std::sync::Arc;

use super::{Certificate, Error, KeyType};

/// Hash algorithm used by a signature scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashAlg {
    /// SHA-256.
    Sha256,
    /// SHA-384.
    Sha384,
    /// SHA-512.
    Sha512,
}

/// Signature encoding and hash used by [`PrivateKey::sign`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignScheme {
    /// RSASSA-PKCS1-v1_5.
    RsaPkcs1(HashAlg),
    /// RSASSA-PSS, matching MGF1 hash and digest-length salt.
    RsaPss(HashAlg),
    /// ECDSA with DER-encoded `(r, s)`.
    Ecdsa(HashAlg),
}

/// A matching certificate/private-key pair; clones share the underlying key.
#[derive(Clone)]
pub struct PrivateKey {
    cert: Certificate,
    #[cfg(not(windows))]
    pub(super) der: Arc<zeroize::Zeroizing<Vec<u8>>>,
    #[cfg(all(not(windows), feature = "rustls"))]
    pub(crate) signing_key: Arc<dyn tls_rustls::sign::SigningKey>,
    #[cfg(all(not(windows), not(feature = "rustls"), feature = "openssl"))]
    key: tls_openssl::pkey::PKey<tls_openssl::pkey::Private>,
    #[cfg(windows)]
    pub(crate) native: super::cng::CngKey,
}

impl PrivateKey {
    /// Imports unencrypted PKCS#8 and verifies that it matches `cert`.
    pub(crate) fn from_pkcs8(cert: &Certificate, der: &[u8]) -> Result<Self, Error> {
        #[cfg(windows)]
        {
            let native = super::cng::from_pkcs8(cert, der)?;
            Ok(Self::from_native(cert.clone(), native))
        }
        #[cfg(all(not(windows), feature = "rustls"))]
        {
            use tls_rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
            let provider = super::crypto::provider()?;
            let key = provider
                .key_provider
                .load_private_key(PrivatePkcs8KeyDer::from(der.to_vec()).into())
                .map_err(|e| Error::InvalidIdentity(e.to_string()))?;
            let certified = tls_rustls::sign::CertifiedKey::new(
                vec![CertificateDer::from(cert.der().to_vec())],
                key.clone(),
            );
            certified
                .keys_match()
                .map_err(|e| Error::InvalidIdentity(e.to_string()))?;
            Ok(Self {
                cert: cert.clone(),
                der: Arc::new(zeroize::Zeroizing::new(der.to_vec())),
                signing_key: key,
            })
        }
        #[cfg(all(not(windows), not(feature = "rustls"), feature = "openssl"))]
        {
            let invalid = |e: tls_openssl::error::ErrorStack| Error::InvalidIdentity(e.to_string());
            let key = tls_openssl::pkey::PKey::private_key_from_pkcs8(der).map_err(invalid)?;
            let public = tls_openssl::pkey::PKey::public_key_from_der(cert.public_key_der())
                .map_err(invalid)?;
            if !key.public_eq(&public) {
                return Err(Error::InvalidIdentity(
                    "certificate/private key mismatch".into(),
                ));
            }
            Ok(Self {
                cert: cert.clone(),
                der: Arc::new(zeroize::Zeroizing::new(der.to_vec())),
                key,
            })
        }
    }

    #[cfg(windows)]
    pub(crate) fn from_native(cert: Certificate, native: super::cng::CngKey) -> Self {
        Self { cert, native }
    }

    /// The public key's recognized algorithm, or `None` for other provider-supported types.
    #[must_use]
    pub fn key_type(&self) -> Option<KeyType> {
        self.cert.key_type()
    }

    /// DER-encoded `SubjectPublicKeyInfo` of the matching certificate.
    #[must_use]
    pub fn public_key_der(&self) -> &[u8] {
        self.cert.public_key_der()
    }

    /// Borrows unencrypted PKCS#8 material. Never log the returned bytes.
    ///
    /// # Errors
    /// [`Error::Unsupported`] for opaque native keys.
    pub fn export_pkcs8_der(&self) -> Result<&[u8], Error> {
        #[cfg(not(windows))]
        {
            Ok(&self.der)
        }
        #[cfg(windows)]
        {
            Err(Error::Unsupported(
                "native private keys cannot be exported".into(),
            ))
        }
    }

    /// Hashes and signs a message using `scheme`.
    ///
    /// # Errors
    /// [`Error::Unsupported`] for an incompatible scheme, or [`Error::Backend`] when signing
    /// fails. Supported algorithms depend on the selected provider.
    pub fn sign(&self, scheme: SignScheme, message: &[u8]) -> Result<Vec<u8>, Error> {
        #[cfg(windows)]
        {
            self.native.sign(scheme, message)
        }
        #[cfg(all(not(windows), feature = "rustls"))]
        {
            self.signing_key
                .choose_scheme(&[scheme.rustls()])
                .ok_or_else(|| Error::Unsupported(format!("signature scheme {scheme:?}")))?
                .sign(message)
                .map_err(|e| Error::Backend(e.to_string()))
        }
        #[cfg(all(not(windows), not(feature = "rustls"), feature = "openssl"))]
        {
            use tls_openssl::hash::MessageDigest;
            use tls_openssl::rsa::Padding;
            use tls_openssl::sign::{RsaPssSaltlen, Signer};

            let rsa = self.key_type() == Some(KeyType::Rsa);
            let hash = match scheme {
                SignScheme::RsaPkcs1(h) | SignScheme::RsaPss(h) if rsa => h,
                SignScheme::Ecdsa(h)
                    if matches!(
                        self.key_type(),
                        Some(KeyType::EcP256 | KeyType::EcP384 | KeyType::EcP521)
                    ) =>
                {
                    h
                }
                _ => return Err(Error::Unsupported(format!("signature scheme {scheme:?}"))),
            };
            let digest = match hash {
                HashAlg::Sha256 => MessageDigest::sha256(),
                HashAlg::Sha384 => MessageDigest::sha384(),
                HashAlg::Sha512 => MessageDigest::sha512(),
            };
            let error = |e: tls_openssl::error::ErrorStack| Error::Backend(e.to_string());
            let mut signer = Signer::new(digest, &self.key).map_err(error)?;
            if matches!(scheme, SignScheme::RsaPss(_)) {
                signer.set_rsa_padding(Padding::PKCS1_PSS).map_err(error)?;
                signer.set_rsa_mgf1_md(digest).map_err(error)?;
                signer
                    .set_rsa_pss_saltlen(RsaPssSaltlen::DIGEST_LENGTH)
                    .map_err(error)?;
            }
            signer.sign_oneshot_to_vec(message).map_err(error)
        }
    }
}

#[cfg(feature = "rustls")]
impl SignScheme {
    pub(crate) fn rustls(self) -> tls_rustls::SignatureScheme {
        use tls_rustls::SignatureScheme as S;
        match self {
            Self::RsaPkcs1(HashAlg::Sha256) => S::RSA_PKCS1_SHA256,
            Self::RsaPkcs1(HashAlg::Sha384) => S::RSA_PKCS1_SHA384,
            Self::RsaPkcs1(HashAlg::Sha512) => S::RSA_PKCS1_SHA512,
            Self::RsaPss(HashAlg::Sha256) => S::RSA_PSS_SHA256,
            Self::RsaPss(HashAlg::Sha384) => S::RSA_PSS_SHA384,
            Self::RsaPss(HashAlg::Sha512) => S::RSA_PSS_SHA512,
            Self::Ecdsa(HashAlg::Sha256) => S::ECDSA_NISTP256_SHA256,
            Self::Ecdsa(HashAlg::Sha384) => S::ECDSA_NISTP384_SHA384,
            Self::Ecdsa(HashAlg::Sha512) => S::ECDSA_NISTP521_SHA512,
        }
    }
}

impl fmt::Debug for PrivateKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PrivateKey")
            .field("key_type", &self.key_type())
            .field("material", &"<redacted>")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::x509::Identity;
    use tls_rustls::SignatureScheme as S;

    fn verify(cert: &Certificate, scheme: S, message: &[u8], signature: &[u8]) -> bool {
        use x509_parser::prelude::{FromDer, X509Certificate};
        let (_, cert) = X509Certificate::from_der(cert.der()).unwrap();
        let provider = tls_rustls::crypto::ring::default_provider();
        provider
            .signature_verification_algorithms
            .mapping
            .iter()
            .find(|(candidate, _)| *candidate == scheme)
            .unwrap()
            .1
            .iter()
            .any(|algorithm| {
                algorithm
                    .verify_signature(
                        &cert.public_key().subject_public_key.data,
                        message,
                        signature,
                    )
                    .is_ok()
            })
    }

    /// STATEMENT: RSA signing uses the requested padding and digest for all supported hashes.
    ///
    /// HOW:
    /// - Sign with PKCS#1 and PSS using SHA-256, SHA-384 and SHA-512.
    /// - Verify with independently selected ring algorithms and reject modified messages.
    /// - Reject each signature under the other padding mode.
    #[test]
    fn rsa_signature_schemes() {
        crate::x509::test_provider();
        let identity = Identity::from_pem(
            include_bytes!("../../examples/cert.pem"),
            include_bytes!("../../examples/key.pem"),
        )
        .unwrap();
        for (hash, pkcs1, pss) in [
            (HashAlg::Sha256, S::RSA_PKCS1_SHA256, S::RSA_PSS_SHA256),
            (HashAlg::Sha384, S::RSA_PKCS1_SHA384, S::RSA_PSS_SHA384),
            (HashAlg::Sha512, S::RSA_PKCS1_SHA512, S::RSA_PSS_SHA512),
        ] {
            for (scheme, expected, other) in [
                (SignScheme::RsaPkcs1(hash), pkcs1, pss),
                (SignScheme::RsaPss(hash), pss, pkcs1),
            ] {
                let signature = identity.private_key().sign(scheme, b"message").unwrap();
                assert!(verify(identity.cert(), expected, b"message", &signature));
                assert!(!verify(identity.cert(), expected, b"modified", &signature));
                assert!(!verify(identity.cert(), other, b"message", &signature));
            }
        }
    }

    /// STATEMENT: EC identities require the matching key and produce interoperable DER signatures.
    ///
    /// HOW:
    /// - Generate P-256 and P-384 identities, rejecting a different key on the same curve.
    /// - Verify each signature and reject RSA schemes for EC keys.
    /// - Export/reload the key, then exercise its rustls adapter when available.
    #[cfg(not(windows))]
    #[test]
    fn ec_identity_signatures() {
        crate::x509::test_provider();
        for (algorithm, hash, scheme) in [
            (
                &rcgen::PKCS_ECDSA_P256_SHA256,
                HashAlg::Sha256,
                S::ECDSA_NISTP256_SHA256,
            ),
            (
                &rcgen::PKCS_ECDSA_P384_SHA384,
                HashAlg::Sha384,
                S::ECDSA_NISTP384_SHA384,
            ),
        ] {
            let key = rcgen::KeyPair::generate_for(algorithm).unwrap();
            let cert = rcgen::CertificateParams::default()
                .self_signed(&key)
                .unwrap();
            let other = rcgen::KeyPair::generate_for(algorithm).unwrap();
            assert!(matches!(
                Identity::from_der(&[cert.der()], other.serialized_der()),
                Err(Error::InvalidIdentity(_))
            ));
            let identity = Identity::from_der(&[cert.der()], key.serialized_der()).unwrap();
            let key = PrivateKey::from_pkcs8(
                identity.cert(),
                identity.private_key().export_pkcs8_der().unwrap(),
            )
            .unwrap();
            let signature = key.sign(SignScheme::Ecdsa(hash), b"message").unwrap();
            assert!(verify(identity.cert(), scheme, b"message", &signature));
            assert!(!verify(identity.cert(), scheme, b"modified", &signature));
            assert!(matches!(
                key.sign(SignScheme::RsaPss(hash), b"message"),
                Err(Error::Unsupported(_))
            ));
            #[cfg(feature = "rustls")]
            {
                let certified = crate::rustls::certified_key(&identity).unwrap();
                assert!(certified.key.choose_scheme(&[S::RSA_PSS_SHA256]).is_none());
                let signer = certified.key.choose_scheme(&[scheme]).unwrap();
                assert_eq!(signer.scheme(), scheme);
                assert!(verify(
                    identity.cert(),
                    scheme,
                    b"message",
                    &signer.sign(b"message").unwrap()
                ));
            }
        }
    }
}
