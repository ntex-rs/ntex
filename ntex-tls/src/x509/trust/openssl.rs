//! OpenSSL trust backend (unix).
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use foreign_types_shared::ForeignTypeRef;
use tls_openssl::asn1::{Asn1Time, Asn1TimeRef};
use tls_openssl::stack::Stack;
use tls_openssl::x509::store::{X509Store, X509StoreBuilder};
use tls_openssl::x509::{X509, X509StoreContext, X509StoreContextRef, verify::X509VerifyFlags};

use super::TrustError;
use crate::x509::{Certificate, PeerCerts};

unsafe extern "C" {
    // Available in all supported OpenSSL versions, not bound by openssl-sys.
    fn X509_STORE_CTX_set_time(
        ctx: *mut openssl_sys::X509_STORE_CTX,
        flags: libc::c_ulong,
        t: libc::time_t,
    );
}

pub(super) struct Builder(X509StoreBuilder);

impl Builder {
    pub(super) fn new() -> Self {
        let mut builder = X509StoreBuilder::new().expect("can allocate memory");
        builder
            .set_flags(X509VerifyFlags::PARTIAL_CHAIN)
            .expect("set_flags is infallible");
        Builder(builder)
    }

    pub(super) fn add(&mut self, cert: &Certificate) -> Result<(), String> {
        let x509 = X509::from_der(cert.der()).map_err(|e| format!("{e:?}"))?;
        self.0.add_cert(x509).map_err(|e| format!("{e:?}"))
    }

    pub(super) fn build(self) -> Store {
        Store(self.0.build())
    }
}

pub(super) struct Store(X509Store);

impl Store {
    pub(super) fn verify(
        &self,
        peer: &PeerCerts,
        now: SystemTime,
    ) -> Result<SystemTime, TrustError> {
        let internal = |e: &dyn std::fmt::Display| {
            TrustError::Internal(format!("Certificate verification failed: {e}").into())
        };

        let leaf = X509::from_der(peer.leaf().der()).map_err(|e| internal(&e))?;
        let mut chain = Stack::new().map_err(|e| internal(&e))?;
        for c in peer.chain() {
            chain
                .push(X509::from_der(c.der()).map_err(|e| internal(&e))?)
                .map_err(|e| internal(&e))?;
        }

        let mut ctx = X509StoreContext::new().map_err(|e| internal(&e))?;
        let verification = ctx
            .init(&self.0, &leaf, &chain, |ctx| {
                set_time(ctx, now);
                ctx.verify_cert().map(|ok| ok.then(|| min_not_after(ctx)))
            })
            .map_err(|e| internal(&e))?;

        match verification {
            Some(Some(expiry)) => Ok(expiry),
            Some(None) => Err(TrustError::Internal(
                "Certificate verification did not return verified chain".into(),
            )),
            None => Err(TrustError::Unauthenticated(
                format!("Peer certificate was rejected: {}", ctx.error()).into(),
            )),
        }
    }
}

fn set_time(ctx: &mut X509StoreContextRef, now: SystemTime) {
    let secs = match now.duration_since(UNIX_EPOCH) {
        Ok(d) => libc::time_t::try_from(d.as_secs()).unwrap_or(libc::time_t::MAX),
        Err(e) => libc::time_t::try_from(e.duration().as_secs()).map_or(libc::time_t::MIN, |s| -s),
    };
    // SAFETY: ctx is an initialized X509_STORE_CTX, set_time only updates its verify params.
    unsafe { X509_STORE_CTX_set_time(ctx.as_ptr(), 0, secs) };
}

fn min_not_after(ctx: &X509StoreContextRef) -> Option<SystemTime> {
    let min = ctx
        .chain()?
        .iter()
        .map(tls_openssl::x509::X509Ref::not_after)
        .reduce(|a, b| if b < a { b } else { a })?;
    asn1_to_system_time(min)
}

fn asn1_to_system_time(t: &Asn1TimeRef) -> Option<SystemTime> {
    let diff = Asn1Time::from_unix(0).ok()?.diff(t).ok()?;
    let secs = i64::from(diff.days) * 86400 + i64::from(diff.secs);
    if secs >= 0 {
        UNIX_EPOCH.checked_add(Duration::from_secs(secs.unsigned_abs()))
    } else {
        UNIX_EPOCH.checked_sub(Duration::from_secs(secs.unsigned_abs()))
    }
}
