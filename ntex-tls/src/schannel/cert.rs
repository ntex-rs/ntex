use std::{fmt, io, ptr, sync::Arc};

use windows_sys::Win32::Security::Cryptography::{
    CERT_CONTEXT, CERT_FIND_SHA1_HASH, CERT_FIND_SUBJECT_STR_W, CERT_KEY_CONTEXT_PROP_ID,
    CERT_KEY_PROV_INFO_PROP_ID, CERT_NCRYPT_KEY_HANDLE_PROP_ID, CERT_STORE_OPEN_EXISTING_FLAG,
    CERT_STORE_PROV_SYSTEM_W, CERT_STORE_READONLY_FLAG, CERT_SYSTEM_STORE_CURRENT_USER,
    CERT_SYSTEM_STORE_LOCAL_MACHINE, CRYPT_INTEGER_BLOB, CertCloseStore,
    CertDuplicateCertificateContext, CertFindCertificateInStore, CertFreeCertificateContext,
    CertGetCertificateContextProperty, CertOpenStore, CertVerifyTimeValidity, HCERTSTORE,
    PKCS_7_ASN_ENCODING, X509_ASN_ENCODING,
};

/// Client certificate with its private key, sent when the server requests one.
///
/// Clones share the certificate.
#[derive(Clone)]
pub struct ClientCert(Arc<CertContext>);

/// Location of a Windows system certificate store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CertStoreLocation {
    /// Stores of the current user, `Cert:\CurrentUser`
    CurrentUser,
    /// Stores of the local machine, `Cert:\LocalMachine`
    LocalMachine,
}

impl ClientCert {
    /// Loads a certificate by its SHA-1 thumbprint from a system store,
    /// for example `"MY"` (Personal).
    ///
    /// # Errors
    ///
    /// Fails if the store cannot be opened, the certificate is not found, or
    /// it has no associated private key.
    pub fn from_store(
        location: CertStoreLocation,
        store: &str,
        thumbprint: &[u8; 20],
    ) -> io::Result<Self> {
        let store = Store::open(location, store)?;

        let hash = CRYPT_INTEGER_BLOB {
            cbData: 20,
            pbData: thumbprint.as_ptr().cast_mut(),
        };
        let cert = unsafe {
            CertFindCertificateInStore(
                store.0,
                X509_ASN_ENCODING | PKCS_7_ASN_ENCODING,
                0,
                CERT_FIND_SHA1_HASH,
                (&raw const hash).cast(),
                ptr::null(),
            )
        };
        if cert.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "client certificate is not found",
            ));
        }
        let cert = Self(Arc::new(CertContext(cert)));
        if has_private_key(cert.as_ptr()) {
            Ok(cert)
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "client certificate has no private key",
            ))
        }
    }

    /// Loads a certificate by its subject name from a system store, for
    /// example `"MY"` (Personal).
    ///
    /// `subject` matches any part of the subject name, case-insensitively,
    /// for example `"client.example.com"` or `"CN=client.example.com"`.
    /// Only currently valid certificates with a private key are considered,
    /// the one that expires last is used.
    ///
    /// # Errors
    ///
    /// Fails if `subject` is empty, the store cannot be opened, or no matching
    /// certificate is found.
    pub fn from_store_by_subject(
        location: CertStoreLocation,
        store: &str,
        subject: &str,
    ) -> io::Result<Self> {
        if subject.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "client certificate subject is empty",
            ));
        }
        let store = Store::open(location, store)?;
        let subject = wide(subject);

        let mut found: Option<(u64, CertContext)> = None;
        let mut cert = ptr::null();
        loop {
            // frees the previous context
            cert = unsafe {
                CertFindCertificateInStore(
                    store.0,
                    X509_ASN_ENCODING | PKCS_7_ASN_ENCODING,
                    0,
                    CERT_FIND_SUBJECT_STR_W,
                    subject.as_ptr().cast(),
                    cert,
                )
            };
            if cert.is_null() {
                break;
            }
            let info = unsafe { (*cert).pCertInfo };
            if unsafe { CertVerifyTimeValidity(ptr::null(), info) } != 0 || !has_private_key(cert) {
                continue;
            }
            let not_after = unsafe { (*info).NotAfter };
            let not_after =
                u64::from(not_after.dwHighDateTime) << 32 | u64::from(not_after.dwLowDateTime);
            if found.as_ref().is_none_or(|(last, _)| not_after > *last) {
                let cert = CertContext(unsafe { CertDuplicateCertificateContext(cert) });
                found = Some((not_after, cert));
            }
        }
        found.map(|(_, cert)| Self(Arc::new(cert))).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "no valid client certificate with a private key matches the subject",
            )
        })
    }

    /// Certificate in DER encoding.
    #[must_use]
    pub fn der(&self) -> &[u8] {
        let cert = unsafe { &*self.as_ptr() };
        unsafe { std::slice::from_raw_parts(cert.pbCertEncoded, cert.cbCertEncoded as usize) }
    }

    pub(super) fn as_ptr(&self) -> *const CERT_CONTEXT {
        self.0.0
    }
}

impl fmt::Debug for ClientCert {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientCert").finish_non_exhaustive()
    }
}

/// Owned certificate context, the context keeps its store alive.
struct CertContext(*const CERT_CONTEXT);

// Certificate contexts are reference counted and immutable here.
unsafe impl Send for CertContext {}
unsafe impl Sync for CertContext {}

impl Drop for CertContext {
    fn drop(&mut self) {
        unsafe {
            CertFreeCertificateContext(self.0);
        }
    }
}

struct Store(HCERTSTORE);

impl Store {
    fn open(location: CertStoreLocation, name: &str) -> io::Result<Self> {
        let location = match location {
            CertStoreLocation::CurrentUser => CERT_SYSTEM_STORE_CURRENT_USER,
            CertStoreLocation::LocalMachine => CERT_SYSTEM_STORE_LOCAL_MACHINE,
        };
        let name = wide(name);
        let store = unsafe {
            CertOpenStore(
                CERT_STORE_PROV_SYSTEM_W,
                0,
                0,
                location | CERT_STORE_READONLY_FLAG | CERT_STORE_OPEN_EXISTING_FLAG,
                name.as_ptr().cast(),
            )
        };
        if store.is_null() {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self(store))
        }
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        // open certificate contexts keep the store alive
        unsafe {
            CertCloseStore(self.0, 0);
        }
    }
}

fn has_private_key(cert: *const CERT_CONTEXT) -> bool {
    [
        CERT_KEY_PROV_INFO_PROP_ID,
        CERT_KEY_CONTEXT_PROP_ID,
        CERT_NCRYPT_KEY_HANDLE_PROP_ID,
    ]
    .into_iter()
    .any(|prop| {
        let mut len = 0u32;
        unsafe { CertGetCertificateContextProperty(cert, prop, ptr::null_mut(), &raw mut len) != 0 }
    })
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_from_store_not_found() {
        let err =
            ClientCert::from_store(CertStoreLocation::CurrentUser, "MY", &[0; 20]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn test_from_store_by_subject_not_found() {
        let err = ClientCert::from_store_by_subject(
            CertStoreLocation::CurrentUser,
            "MY",
            "CN=ntex no such subject 3f0c",
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);

        let err = ClientCert::from_store_by_subject(CertStoreLocation::CurrentUser, "MY", "")
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
}
