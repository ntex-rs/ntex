//! Private certificate-signature operations for the selected X.509 backend.
use super::{Certificate, Error};

/// Uses the application's provider, or ring when explicitly enabled by this crate.
#[cfg(all(not(windows), feature = "rustls"))]
pub(crate) fn provider() -> Result<std::sync::Arc<tls_rustls::crypto::CryptoProvider>, Error> {
    let provider = tls_rustls::crypto::CryptoProvider::get_default().cloned();
    #[cfg(feature = "rustls-ring")]
    let provider = provider.or_else(|| {
        Some(std::sync::Arc::new(
            tls_rustls::crypto::ring::default_provider(),
        ))
    });
    provider.ok_or_else(|| {
        Error::Backend("install a rustls CryptoProvider or enable rustls-ring".into())
    })
}

/// Checks a certificate signature, without imposing trust or validity policy.
pub(crate) fn verify_signature(cert: &Certificate, issuer: &Certificate) -> Result<bool, Error> {
    #[cfg(windows)]
    {
        super::cng::verify_signature(cert, issuer)
    }
    #[cfg(all(not(windows), not(feature = "rustls"), feature = "openssl"))]
    {
        use tls_openssl::x509::X509;
        let invalid = |e: tls_openssl::error::ErrorStack| Error::InvalidIdentity(e.to_string());
        let cert = X509::from_der(cert.der()).map_err(invalid)?;
        let issuer = X509::from_der(issuer.der()).map_err(invalid)?;
        let key = issuer.public_key().map_err(invalid)?;
        Ok(cert.verify(&key).unwrap_or(false))
    }
    #[cfg(all(not(windows), feature = "rustls"))]
    {
        use x509_parser::asn1_rs::Any;
        use x509_parser::prelude::{FromDer, X509Certificate};

        let (_, parsed) =
            X509Certificate::from_der(cert.der()).map_err(|e| Error::InvalidDer(e.to_string()))?;
        let (_, issuer) = X509Certificate::from_der(issuer.der())
            .map_err(|e| Error::InvalidDer(e.to_string()))?;
        if parsed.signature_algorithm != parsed.tbs_certificate.signature {
            return Ok(false);
        }
        let data = |der| Any::from_der(der).map_err(|e| Error::InvalidDer(e.to_string()));
        let (_, outer) = data(cert.der())?;
        let (rest, _) = data(outer.data)?;
        let (_, signature_algorithm) = data(rest)?;
        let (_, spki) = data(issuer.public_key().raw)?;
        let (_, key_algorithm) = data(spki.data)?;
        Ok(provider()?
            .signature_verification_algorithms
            .all
            .iter()
            .any(|algorithm| {
                algorithm.signature_alg_id().as_ref() == signature_algorithm.data
                    && algorithm.public_key_alg_id().as_ref() == key_algorithm.data
                    && algorithm
                        .verify_signature(
                            &issuer.public_key().subject_public_key.data,
                            parsed.tbs_certificate.as_ref(),
                            &parsed.signature_value.data,
                        )
                        .is_ok()
            }))
    }
}

/// Matches issuer metadata before probing a signature; this is not trust verification.
#[cfg(all(not(windows), not(feature = "rustls"), feature = "openssl"))]
pub(crate) fn is_issuer(cert: &Certificate, issuer: &Certificate) -> Result<bool, Error> {
    use tls_openssl::x509::{X509, X509VerifyResult};
    let invalid = |e: tls_openssl::error::ErrorStack| Error::InvalidIdentity(e.to_string());
    let child = X509::from_der(cert.der()).map_err(invalid)?;
    let parent = X509::from_der(issuer.der()).map_err(invalid)?;
    Ok(parent.issued(&child) == X509VerifyResult::OK && verify_signature(cert, issuer)?)
}

/// Matches issuer metadata before probing a signature; this is not trust verification.
#[cfg(any(windows, feature = "rustls"))]
pub(crate) fn is_issuer(cert: &Certificate, issuer: &Certificate) -> Result<bool, Error> {
    use x509_parser::extensions::{GeneralName, ParsedExtension};
    use x509_parser::prelude::{FromDer, X509Certificate};

    let (_, parsed) =
        X509Certificate::from_der(cert.der()).map_err(|e| Error::InvalidDer(e.to_string()))?;
    let (_, parent) =
        X509Certificate::from_der(issuer.der()).map_err(|e| Error::InvalidDer(e.to_string()))?;
    if parsed.issuer() != parent.subject() {
        return Ok(false);
    }
    if parent.key_usage().is_err()
        || parent
            .key_usage()
            .ok()
            .flatten()
            .is_some_and(|ku| !ku.value.key_cert_sign())
    {
        return Ok(false);
    }
    for extension in parsed.extensions() {
        if let ParsedExtension::AuthorityKeyIdentifier(aki) = extension.parsed_extension() {
            if let Some(serial) = aki.authority_cert_serial
                && serial != parent.raw_serial()
            {
                return Ok(false);
            }
            if let Some(names) = &aki.authority_cert_issuer
                && !names.iter().any(|name| {
                    matches!(name, GeneralName::DirectoryName(name) if name == parent.issuer())
                })
            {
                return Ok(false);
            }
            if let Some(id) = &aki.key_identifier {
                for extension in parent.extensions() {
                    if let ParsedExtension::SubjectKeyIdentifier(ski) = extension.parsed_extension()
                        && id != ski
                    {
                        return Ok(false);
                    }
                }
            }
        }
    }
    verify_signature(cert, issuer)
}

/// Self-issuance is only a root candidate until its self-signature verifies.
pub(crate) fn is_self_signed(cert: &Certificate) -> Result<bool, Error> {
    #[cfg(all(not(windows), not(feature = "rustls"), feature = "openssl"))]
    let self_issued = {
        use tls_openssl::x509::X509;
        let invalid = |e: tls_openssl::error::ErrorStack| Error::InvalidIdentity(e.to_string());
        let cert = X509::from_der(cert.der()).map_err(invalid)?;
        cert.subject_name()
            .try_cmp(cert.issuer_name())
            .map_err(invalid)?
            .is_eq()
    };
    #[cfg(any(windows, feature = "rustls"))]
    let self_issued = cert.is_self_issued();
    Ok(self_issued && verify_signature(cert, cert)?)
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;
    use rcgen::{
        BasicConstraints, CertificateParams, CustomExtension, DnType, IsCa, Issuer, KeyIdMethod,
        KeyPair, KeyUsagePurpose,
    };

    /// STATEMENT: A valid signature alone cannot establish an issuer relationship.
    ///
    /// HOW:
    /// - Sign a leaf with a CA key and require matching SKI/AKI and certificate-signing usage.
    /// - Reject same-key candidates with the wrong SKI or key usage, despite valid signatures.
    /// - Reject signed leaves whose AKI serial or issuer name disagrees with the candidate.
    #[test]
    fn issuer_metadata() {
        crate::x509::test_provider();
        let key = KeyPair::generate().unwrap();
        let mut ca = CertificateParams::default();
        ca.distinguished_name.push(DnType::CommonName, "CA");
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        ca.key_identifier_method = KeyIdMethod::PreSpecified(vec![1]);
        ca.serial_number = Some(7u64.into());
        let parent = Certificate::from_der(ca.self_signed(&key).unwrap().der().to_vec()).unwrap();
        let leaf_key = KeyPair::generate().unwrap();
        let mut leaf = CertificateParams::default();
        leaf.use_authority_key_identifier_extension = true;
        let cert = Certificate::from_der(
            leaf.signed_by(&leaf_key, &Issuer::from_params(&ca, &key))
                .unwrap()
                .der()
                .to_vec(),
        )
        .unwrap();
        assert!(is_issuer(&cert, &parent).unwrap());
        ca.key_identifier_method = KeyIdMethod::PreSpecified(vec![2]);
        let wrong_ski =
            Certificate::from_der(ca.self_signed(&key).unwrap().der().to_vec()).unwrap();
        assert!(verify_signature(&cert, &wrong_ski).unwrap());
        assert!(!is_issuer(&cert, &wrong_ski).unwrap());
        ca.key_identifier_method = KeyIdMethod::PreSpecified(vec![1]);
        ca.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        let wrong_usage =
            Certificate::from_der(ca.self_signed(&key).unwrap().der().to_vec()).unwrap();
        assert!(verify_signature(&cert, &wrong_usage).unwrap());
        assert!(!is_issuer(&cert, &wrong_usage).unwrap());
        ca.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        leaf.use_authority_key_identifier_extension = false;
        for (issuer, serial, accepted) in [(b'A', 7, true), (b'A', 8, false), (b'B', 7, false)] {
            // AKI with directoryName(CN=CA/CB) and authorityCertSerialNumber=7/8.
            let aki = vec![
                0x30, 22, 0xa1, 17, 0xa4, 15, 0x30, 13, 0x31, 11, 0x30, 9, 0x06, 3, 0x55, 4, 3,
                0x0c, 2, b'C', issuer, 0x82, 1, serial,
            ];
            leaf.custom_extensions = vec![CustomExtension::from_oid_content(&[2, 5, 29, 35], aki)];
            let cert = Certificate::from_der(
                leaf.signed_by(&leaf_key, &Issuer::from_params(&ca, &key))
                    .unwrap()
                    .der()
                    .to_vec(),
            )
            .unwrap();
            assert!(verify_signature(&cert, &parent).unwrap());
            assert_eq!(
                is_issuer(&cert, &parent).unwrap(),
                accepted,
                "issuer CN=C{}, serial={serial}",
                char::from(issuer)
            );
        }
    }
}
