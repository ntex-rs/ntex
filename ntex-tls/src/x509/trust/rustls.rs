//! `WebPKI` path verification using the application's rustls crypto provider.
use std::time::{SystemTime, UNIX_EPOCH};

use tls_rustls::pki_types::{CertificateDer, UnixTime};

use super::TrustError;
use crate::x509::{Certificate, PeerCerts, crypto};

pub(super) struct Builder(Vec<Certificate>);

impl Builder {
    pub(super) fn new() -> Self {
        Self(Vec::new())
    }

    pub(super) fn add(&mut self, cert: &Certificate) -> Result<(), String> {
        webpki::anchor_from_trusted_cert(&CertificateDer::from(cert.der().as_ref()))
            .map_err(|e| e.to_string())?;
        self.0.push(cert.clone());
        Ok(())
    }

    pub(super) fn build(self) -> Store {
        Store(self.0)
    }
}

pub(super) struct Store(Vec<Certificate>);

impl Store {
    pub(super) fn verify(
        &self,
        peer: &PeerCerts,
        now: SystemTime,
    ) -> Result<SystemTime, TrustError> {
        let rejected = |e: String| TrustError::Unauthenticated(e.into());
        let time = now.duration_since(UNIX_EPOCH).map_err(|_| {
            TrustError::Internal("WebPKI cannot verify at times before 1970".into())
        })?;
        if self.0.contains(peer.leaf()) {
            return if peer.leaf().not_before() <= now && now <= peer.leaf().not_after() {
                Ok(peer.leaf().not_after())
            } else {
                Err(rejected(
                    "Pinned certificate is outside its validity period".into(),
                ))
            };
        }
        let provider =
            crypto::provider().map_err(|e| TrustError::Internal(e.to_string().into()))?;
        // WebPKI trust anchors omit dates. Only offer currently valid anchors to path building.
        let valid: Vec<_> = self
            .0
            .iter()
            .filter(|cert| cert.not_before() <= now && now <= cert.not_after())
            .collect();
        let anchor_der: Vec<_> = valid
            .iter()
            .map(|cert| CertificateDer::from(cert.der().as_ref()))
            .collect();
        let anchors = anchor_der
            .iter()
            .map(webpki::anchor_from_trusted_cert)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| TrustError::Internal(e.to_string().into()))?;
        let intermediates: Vec<_> = peer
            .chain()
            .iter()
            .map(|cert| CertificateDer::from(cert.der().as_ref()))
            .collect();
        let leaf_der = CertificateDer::from(peer.leaf().der().as_ref());
        let leaf =
            webpki::EndEntityCert::try_from(&leaf_der).map_err(|e| rejected(e.to_string()))?;
        let path = leaf
            .verify_for_usage(
                provider.signature_verification_algorithms.all,
                &anchors,
                &intermediates,
                UnixTime::since_unix_epoch(time),
                webpki::KeyUsage::client_auth(),
                None,
                None,
            )
            .map_err(|e| rejected(e.to_string()))?;
        let index = anchors
            .iter()
            .position(|anchor| std::ptr::eq(anchor, path.anchor()))
            .ok_or_else(|| TrustError::Internal("Verified anchor is missing".into()))?;
        let mut expiry = peer.leaf().not_after().min(valid[index].not_after());
        for cert in path.intermediate_certificates() {
            let cert = peer
                .chain()
                .iter()
                .find(|candidate| candidate.der().as_ref() == cert.der().as_ref())
                .ok_or_else(|| TrustError::Internal("Verified intermediate is missing".into()))?;
            expiry = expiry.min(cert.not_after());
        }
        Ok(expiry)
    }
}
