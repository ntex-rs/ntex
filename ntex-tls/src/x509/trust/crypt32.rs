//! Crypt32 trust backend (Windows).
//!
//! Chain building, signature verification (CNG) and validity checks are done by
//! a private chain engine (`CertCreateCertificateChainEngine`):
//! - `hExclusiveRoot` holds only the registered certificates, the Windows root store is never
//!   consulted; with `CERT_CHAIN_EXCLUSIVE_ENABLE_CA_FLAG` registered intermediate CAs are trust
//!   anchors too (partial chain);
//! - Windows rejects `hRestrictedOther`/`hRestrictedTrust` together with `hExclusiveRoot`
//!   (`E_INVALIDARG`), so the engine may look up intermediates in the system stores; a built
//!   chain containing a certificate that is neither sent by the peer nor registered is rejected;
//! - no network access (`CERT_CHAIN_DISABLE_AIA`, cache-only URL retrieval) and no revocation
//!   checks;
//! - the engine does not check the validity period of the trust anchor, the validity of every
//!   certificate on the built chain is checked against `now` after chain building;
//! - the client authentication usage is requested, a certificate with an extended key usage
//!   that does not contain `clientAuth` invalidates the chain.
//!
//! Weak signatures (e.g. SHA-1) follow the Windows weak crypto policy; by default SHA-1 is
//! accepted for chains that end in a non-Microsoft root.
use std::{mem, ptr, time::Duration, time::SystemTime, time::UNIX_EPOCH};

use windows_sys::Win32::Foundation::FILETIME;
use windows_sys::Win32::Security::Cryptography::{
    CERT_CHAIN_CACHE_ONLY_URL_RETRIEVAL, CERT_CHAIN_DISABLE_AIA, CERT_CHAIN_ENGINE_CONFIG,
    CERT_CHAIN_EXCLUSIVE_ENABLE_CA_FLAG, CERT_CHAIN_PARA,
    CERT_TRUST_HAS_NOT_SUPPORTED_CRITICAL_EXT, CERT_TRUST_HAS_WEAK_SIGNATURE,
    CERT_TRUST_INVALID_BASIC_CONSTRAINTS, CERT_TRUST_IS_CYCLIC, CERT_TRUST_IS_NOT_SIGNATURE_VALID,
    CERT_TRUST_IS_NOT_TIME_VALID, CERT_TRUST_IS_NOT_VALID_FOR_USAGE, CERT_TRUST_IS_PARTIAL_CHAIN,
    CERT_TRUST_IS_UNTRUSTED_ROOT, CERT_TRUST_NO_ERROR, CERT_USAGE_MATCH, CTL_USAGE,
    CertCreateCertificateChainEngine, CertFreeCertificateChainEngine, HCERTCHAINENGINE,
    USAGE_MATCH_TYPE_AND, szOID_PKIX_KP_CLIENT_AUTH,
};

use super::TrustError;
use crate::win::{CertStore, ChainContext};
use crate::x509::{Certificate, PeerCerts};

/// Seconds between 1601-01-01 (`FILETIME` epoch) and 1970-01-01.
const FILETIME_UNIX_OFFSET: u64 = 11_644_473_600;

/// Names for `CERT_TRUST_STATUS::dwErrorStatus` bits, used in error messages.
const ERRORS: &[(u32, &str)] = &[
    (CERT_TRUST_IS_NOT_TIME_VALID, "NotTimeValid"),
    (CERT_TRUST_IS_NOT_SIGNATURE_VALID, "BadSignature"),
    (CERT_TRUST_IS_NOT_VALID_FOR_USAGE, "NotValidForUsage"),
    (CERT_TRUST_IS_UNTRUSTED_ROOT, "UnknownIssuer"),
    (CERT_TRUST_IS_CYCLIC, "Cyclic"),
    (
        CERT_TRUST_INVALID_BASIC_CONSTRAINTS,
        "InvalidBasicConstraints",
    ),
    (CERT_TRUST_IS_PARTIAL_CHAIN, "PartialChain"),
    (CERT_TRUST_HAS_WEAK_SIGNATURE, "WeakSignature"),
    (
        CERT_TRUST_HAS_NOT_SUPPORTED_CRITICAL_EXT,
        "UnsupportedCriticalExtension",
    ),
];

fn memory_store() -> Result<CertStore, String> {
    CertStore::memory().map_err(|e| format!("CertOpenStore failed: {e}"))
}

pub(super) struct Builder {
    roots: Result<CertStore, String>,
    anchors: Vec<Certificate>,
}

impl Builder {
    pub(super) fn new() -> Self {
        Builder {
            roots: memory_store(),
            anchors: Vec::new(),
        }
    }

    pub(super) fn add(&mut self, cert: &Certificate) -> Result<(), String> {
        let roots = self.roots.as_ref().map_err(Clone::clone)?;
        roots
            .add_encoded_only(cert.der())
            .map_err(|e| e.to_string())?;
        self.anchors.push(cert.clone());
        Ok(())
    }

    pub(super) fn build(self) -> Store {
        let engine = self.roots.and_then(Engine::new);
        if let Err(e) = &engine {
            log::error!("Unable to create certificate chain engine: {e}");
        }
        Store {
            engine,
            anchors: self.anchors,
        }
    }
}

/// Private chain engine, see the module docs.
///
/// Owns the root store it was configured with; the handle is freed before the store closes.
struct Engine {
    handle: HCERTCHAINENGINE,
    _roots: CertStore,
}

// SAFETY: chain engines are thread safe, the handle is only freed on drop.
unsafe impl Send for Engine {}
// SAFETY: see `Send`.
unsafe impl Sync for Engine {}

impl Engine {
    fn new(roots: CertStore) -> Result<Self, String> {
        let config = CERT_CHAIN_ENGINE_CONFIG {
            cbSize: u32::try_from(mem::size_of::<CERT_CHAIN_ENGINE_CONFIG>()).expect("fits u32"),
            hRestrictedRoot: ptr::null_mut(),
            // must be null with `hExclusiveRoot`
            hRestrictedTrust: ptr::null_mut(),
            hRestrictedOther: ptr::null_mut(),
            cAdditionalStore: 0,
            rghAdditionalStore: ptr::null_mut(),
            dwFlags: CERT_CHAIN_CACHE_ONLY_URL_RETRIEVAL,
            dwUrlRetrievalTimeout: 0,
            MaximumCachedCertificates: 0,
            CycleDetectionModulus: 0,
            hExclusiveRoot: roots.as_raw(),
            hExclusiveTrustedPeople: ptr::null_mut(),
            dwExclusiveFlags: CERT_CHAIN_EXCLUSIVE_ENABLE_CA_FLAG,
        };
        let mut h: HCERTCHAINENGINE = ptr::null_mut();
        // SAFETY: config is fully initialized, the root store outlives the engine.
        if unsafe { CertCreateCertificateChainEngine(&raw const config, &raw mut h) } == 0 {
            Err(format!(
                "CertCreateCertificateChainEngine failed: {}",
                std::io::Error::last_os_error()
            ))
        } else {
            Ok(Engine {
                handle: h,
                _roots: roots,
            })
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        // SAFETY: handle is owned.
        unsafe { CertFreeCertificateChainEngine(self.handle) };
    }
}

pub(super) struct Store {
    engine: Result<Engine, String>,
    anchors: Vec<Certificate>,
}

fn rejected(e: impl std::fmt::Display) -> TrustError {
    TrustError::Unauthenticated(format!("Peer certificate was rejected: {e}").into())
}

fn internal(e: impl std::fmt::Display) -> TrustError {
    TrustError::Internal(format!("Certificate verification failed: {e}").into())
}

#[allow(clippy::cast_possible_truncation)] // splits the 64-bit tick count into two halves
fn filetime(t: SystemTime) -> FILETIME {
    let epoch = UNIX_EPOCH - Duration::from_secs(FILETIME_UNIX_OFFSET);
    let d = t
        .duration_since(epoch)
        .expect("Windows SystemTime is within FILETIME range");
    let ticks = d.as_secs() * 10_000_000 + u64::from(d.subsec_nanos() / 100);
    FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    }
}

fn error_names(status: u32) -> String {
    let mut names: Vec<&str> = ERRORS
        .iter()
        .filter(|(bit, _)| status & bit != 0)
        .map(|(_, n)| *n)
        .collect();
    if names.is_empty() {
        names.push("Other");
    }
    format!("{} (0x{status:08x})", names.join("|"))
}

impl Store {
    pub(super) fn verify(
        &self,
        peer: &PeerCerts,
        now: SystemTime,
    ) -> Result<SystemTime, TrustError> {
        let leaf = peer.leaf();

        // Pinned leaf: an end-entity certificate is not a chain-engine trust anchor.
        if self.anchors.iter().any(|a| a.der() == leaf.der()) {
            if now < leaf.not_before() || now > leaf.not_after() {
                return Err(rejected(error_names(CERT_TRUST_IS_NOT_TIME_VALID)));
            }
            return Ok(leaf.not_after());
        }

        let engine = self.engine.as_ref().map_err(internal)?;
        let extra = memory_store().map_err(internal)?;
        let leaf_ctx = extra.add_encoded(leaf.der()).map_err(rejected)?;
        for c in peer.chain() {
            extra.add_encoded_only(c.der()).map_err(rejected)?;
        }

        let mut usage = [szOID_PKIX_KP_CLIENT_AUTH.cast_mut()];
        // SAFETY: all-zero is a valid `CERT_CHAIN_PARA` (no optional parameters).
        let mut para: CERT_CHAIN_PARA = unsafe { mem::zeroed() };
        para.cbSize = u32::try_from(mem::size_of::<CERT_CHAIN_PARA>()).expect("fits u32");
        para.RequestedUsage = CERT_USAGE_MATCH {
            dwType: USAGE_MATCH_TYPE_AND,
            Usage: CTL_USAGE {
                cUsageIdentifier: 1,
                rgpszUsageIdentifier: usage.as_mut_ptr(),
            },
        };
        let time = filetime(now);
        // SAFETY: the engine is owned by `self`, `usage` outlives the call.
        let chain = unsafe {
            ChainContext::build(
                engine.handle,
                &leaf_ctx,
                Some(&time),
                Some(&extra),
                &para,
                CERT_CHAIN_DISABLE_AIA,
            )
        }
        .map_err(internal)?;

        let status = chain.error_status();
        if status != CERT_TRUST_NO_ERROR {
            return Err(rejected(error_names(status)));
        }

        let mut expiry: Option<SystemTime> = None;
        for cert in chain.certificates() {
            let der = cert.der();
            let cert = std::iter::once(leaf)
                .chain(peer.chain())
                .chain(&self.anchors)
                .find(|c| c.der().as_ref() == der)
                .ok_or_else(|| rejected("chain uses a certificate not sent by the peer"))?;
            // the chain engine does not check the validity of trust anchors
            if now < cert.not_before() || now > cert.not_after() {
                return Err(rejected(error_names(CERT_TRUST_IS_NOT_TIME_VALID)));
            }
            let not_after = cert.not_after();
            expiry = Some(expiry.map_or(not_after, |e| e.min(not_after)));
        }
        expiry.ok_or_else(|| internal("empty chain"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// STATEMENT: Windows verification times preserve dates and subsecond values before 1970.
    ///
    /// HOW:
    /// - Convert the Windows epoch and times immediately before and after the Unix epoch.
    /// - Check the resulting `FILETIME` tick counts, including 100 ns precision.
    #[test]
    fn filetime_before_unix_epoch() {
        let unix_ticks = FILETIME_UNIX_OFFSET * 10_000_000;
        for (time, ticks) in [
            (UNIX_EPOCH - Duration::from_secs(FILETIME_UNIX_OFFSET), 0),
            (
                UNIX_EPOCH - Duration::from_mins(1),
                unix_ticks - 600_000_000,
            ),
            (UNIX_EPOCH - Duration::from_nanos(100), unix_ticks - 1),
            (UNIX_EPOCH, unix_ticks),
            (UNIX_EPOCH + Duration::from_nanos(100), unix_ticks + 1),
        ] {
            let ft = filetime(time);
            assert_eq!(
                (u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime),
                ticks
            );
        }
    }
}
