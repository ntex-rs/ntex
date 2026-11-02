//! Adapters from portable identities to rustls signing keys, including opaque CNG keys.
use tls_rustls::pki_types::CertificateDer;
use tls_rustls::sign::CertifiedKey;

use crate::x509::{ClientIdentity, Error, Identity};

/// Packages an identity for a rustls client/server certificate resolver.
///
/// The chain is ordered and excludes the self-signed root. Native keys sign in place.
///
/// # Errors
/// See [`Identity::ordered_chain`].
pub fn certified_key(identity: &Identity) -> Result<CertifiedKey, Error> {
    let client = ClientIdentity::from_identity(identity.clone())?;
    Ok(client_certified_key(&client))
}

/// Packages an already ordered client identity for a rustls certificate resolver.
///
/// On Windows the adapter retains the native key owner; no private key is exported.
#[must_use]
pub fn client_certified_key(identity: &ClientIdentity) -> CertifiedKey {
    let chain = identity
        .chain()
        .iter()
        .map(|cert| CertificateDer::from(cert.der().to_vec()))
        .collect();
    #[cfg(not(windows))]
    let key = identity.private_key().signing_key.clone();
    #[cfg(windows)]
    let key = std::sync::Arc::new(NativeKey(identity.private_key().clone()));
    CertifiedKey::new(chain, key)
}

#[cfg(windows)]
#[derive(Debug)]
struct NativeKey(crate::x509::PrivateKey);

#[cfg(windows)]
impl tls_rustls::sign::SigningKey for NativeKey {
    fn choose_scheme(
        &self,
        offered: &[tls_rustls::SignatureScheme],
    ) -> Option<Box<dyn tls_rustls::sign::Signer>> {
        use crate::x509::{HashAlg as H, KeyType as K, SignScheme as S};
        let schemes: &[S] = match self.0.key_type()? {
            K::Rsa => &[
                S::RsaPss(H::Sha256),
                S::RsaPss(H::Sha384),
                S::RsaPss(H::Sha512),
                S::RsaPkcs1(H::Sha256),
                S::RsaPkcs1(H::Sha384),
                S::RsaPkcs1(H::Sha512),
            ],
            K::EcP256 => &[S::Ecdsa(H::Sha256)],
            K::EcP384 => &[S::Ecdsa(H::Sha384)],
            K::EcP521 => &[S::Ecdsa(H::Sha512)],
        };
        let scheme = schemes
            .iter()
            .find(|scheme| offered.contains(&scheme.rustls()))?;
        Some(Box::new(NativeSigner {
            key: self.0.clone(),
            scheme: *scheme,
        }))
    }

    fn public_key(&self) -> Option<tls_rustls::pki_types::SubjectPublicKeyInfoDer<'_>> {
        Some(self.0.public_key_der().into())
    }

    fn algorithm(&self) -> tls_rustls::SignatureAlgorithm {
        if self.0.key_type() == Some(crate::x509::KeyType::Rsa) {
            tls_rustls::SignatureAlgorithm::RSA
        } else {
            tls_rustls::SignatureAlgorithm::ECDSA
        }
    }
}

#[cfg(windows)]
#[derive(Debug)]
struct NativeSigner {
    key: crate::x509::PrivateKey,
    scheme: crate::x509::SignScheme,
}

#[cfg(windows)]
impl tls_rustls::sign::Signer for NativeSigner {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, tls_rustls::Error> {
        self.key
            .sign(self.scheme, message)
            .map_err(|e| tls_rustls::Error::General(e.to_string()))
    }

    fn scheme(&self) -> tls_rustls::SignatureScheme {
        self.scheme.rustls()
    }
}
