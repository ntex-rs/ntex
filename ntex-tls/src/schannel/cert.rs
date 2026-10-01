use std::{fmt, fmt::Write, io, ptr, sync::Arc};

use windows_sys::Win32::Security::Cryptography::{
    BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptBuffer, BCryptBufferDesc, BCryptGenRandom, CERT_CONTEXT,
    CERT_FIND_SHA1_HASH, CERT_FIND_SUBJECT_STR_W, CERT_KEY_CONTEXT_PROP_ID,
    CERT_KEY_PROV_INFO_PROP_ID, CERT_NCRYPT_KEY_HANDLE_PROP_ID, CERT_STORE_OPEN_EXISTING_FLAG,
    CERT_STORE_PROV_SYSTEM_W, CERT_STORE_READONLY_FLAG, CERT_SYSTEM_STORE_CURRENT_USER,
    CERT_SYSTEM_STORE_LOCAL_MACHINE, CRYPT_EXPORTABLE, CRYPT_INTEGER_BLOB, CRYPT_KEY_PROV_INFO,
    CertCloseStore, CertDuplicateCertificateContext, CertEnumCertificatesInStore,
    CertFindCertificateInStore, CertFreeCertificateContext, CertGetCertificateContextProperty,
    CertOpenStore, CertSetCertificateContextProperty, CertVerifyTimeValidity, HCERTSTORE,
    MS_KEY_STORAGE_PROVIDER, NCRYPT_ALLOW_EXPORT_FLAG, NCRYPT_ALLOW_PLAINTEXT_EXPORT_FLAG,
    NCRYPT_EXPORT_POLICY_PROPERTY, NCRYPT_KEY_HANDLE, NCRYPT_MACHINE_KEY_FLAG,
    NCRYPT_PKCS8_PRIVATE_KEY_BLOB, NCRYPT_PROV_HANDLE, NCRYPT_SILENT_FLAG,
    NCRYPTBUFFER_PKCS_KEY_NAME, NCRYPTBUFFER_VERSION, NCryptDeleteKey, NCryptExportKey,
    NCryptFreeObject, NCryptImportKey, NCryptOpenKey, NCryptOpenStorageProvider, NCryptSetProperty,
    PFXImportCertStore, PKCS_7_ASN_ENCODING, PKCS12_ALWAYS_CNG_KSP, PKCS12_NO_PERSIST_KEY,
    X509_ASN_ENCODING,
};

/// Certificate with its private key.
///
/// Identifies a server, see [`ServerConfig`](super::ServerConfig), or a
/// client, see [`ClientConfig::set_client_cert()`](super::ClientConfig::set_client_cert).
///
/// Clones share the certificate.
#[derive(Clone)]
pub struct Certificate(Arc<CertContext>);

/// Location of a Windows system certificate store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CertStoreLocation {
    /// Stores of the current user, `Cert:\CurrentUser`
    CurrentUser,
    /// Stores of the local machine, `Cert:\LocalMachine`
    LocalMachine,
}

impl Certificate {
    /// Loads a certificate and its private key from a PKCS #12 (PFX) archive
    /// in DER encoding.
    ///
    /// Schannel requires a persisted private key, the key is stored in the
    /// current user's key store under a unique name and deleted when the
    /// certificate and all configurations using it are dropped. The key is
    /// left behind if the process does not exit normally.
    ///
    /// If the archive holds multiple certificates, the first one with a
    /// private key is used, the others are available to build its chain.
    ///
    /// # Errors
    ///
    /// Fails if the archive cannot be decoded with `password`, has no
    /// certificate with a private key, or the key cannot be stored.
    pub fn from_pkcs12(der: &[u8], password: &str) -> io::Result<Self> {
        let blob = CRYPT_INTEGER_BLOB {
            cbData: u32::try_from(der.len()).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "PKCS #12 archive is too large")
            })?,
            pbData: der.as_ptr().cast_mut(),
        };
        let password = wide(password);
        // a key persisted by the import is named after the archive's friendly
        // name, imports of the same archive would share and delete it; the key
        // is imported to memory and persisted under a unique name instead
        let flags = PKCS12_NO_PERSIST_KEY | PKCS12_ALWAYS_CNG_KSP | CRYPT_EXPORTABLE;
        let store = unsafe { PFXImportCertStore(&raw const blob, password.as_ptr(), flags) };
        if store.is_null() {
            return Err(io::Error::last_os_error());
        }
        let store = Store(store);

        let mut cert = ptr::null();
        loop {
            // frees the previous context
            cert = unsafe { CertEnumCertificatesInStore(store.0, cert) };
            if cert.is_null() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "PKCS #12 archive has no certificate with a private key",
                ));
            }
            if has_private_key(cert) {
                break;
            }
        }
        let mut cert = CertContext::new(cert);
        persist_key(cert.cert)?;
        cert.temp_key = true;
        Ok(Self(Arc::new(cert)))
    }

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
                "certificate is not found",
            ));
        }
        let cert = Self(Arc::new(CertContext::new(cert)));
        if has_private_key(cert.as_ptr()) {
            Ok(cert)
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "certificate has no private key",
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
                "certificate subject is empty",
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
                let cert = CertContext::new(unsafe { CertDuplicateCertificateContext(cert) });
                found = Some((not_after, cert));
            }
        }
        found.map(|(_, cert)| Self(Arc::new(cert))).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "no valid certificate with a private key matches the subject",
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
        self.0.cert
    }

    /// Takes ownership of a certificate context.
    #[cfg(test)]
    pub(super) fn from_context(cert: *const CERT_CONTEXT) -> Self {
        Self(Arc::new(CertContext::new(cert)))
    }
}

impl fmt::Debug for Certificate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Certificate").finish_non_exhaustive()
    }
}

/// Owned certificate context, the context keeps its store alive.
struct CertContext {
    cert: *const CERT_CONTEXT,
    /// The private key was imported for this context and is deleted with it
    temp_key: bool,
}

impl CertContext {
    fn new(cert: *const CERT_CONTEXT) -> Self {
        Self {
            cert,
            temp_key: false,
        }
    }
}

// Certificate contexts are reference counted and immutable here.
unsafe impl Send for CertContext {}
unsafe impl Sync for CertContext {}

impl Drop for CertContext {
    fn drop(&mut self) {
        if self.temp_key {
            delete_key(self.cert);
        }
        unsafe {
            CertFreeCertificateContext(self.cert);
        }
    }
}

/// Persists the in-memory private key of a certificate under a unique name,
/// the certificate refers to the persisted key afterwards.
fn persist_key(cert: *const CERT_CONTEXT) -> io::Result<()> {
    let pkcs8 = export_key(cert)?;

    let mut id = [0u8; 16];
    let status =
        unsafe { BCryptGenRandom(0 as _, id.as_mut_ptr(), 16, BCRYPT_USE_SYSTEM_PREFERRED_RNG) };
    if status != 0 {
        return Err(io::Error::other("BCryptGenRandom failed"));
    }
    let name = id.iter().fold(String::from("ntex-tls-"), |mut name, b| {
        let _ = write!(name, "{b:02x}");
        name
    });
    let name = wide(&name);

    let prov = Provider::open(MS_KEY_STORAGE_PROVIDER)?;
    let mut name_buf = BCryptBuffer {
        cbBuffer: u32::try_from(name.len() * 2).expect("key name fits u32"),
        BufferType: NCRYPTBUFFER_PKCS_KEY_NAME,
        pvBuffer: name.as_ptr().cast_mut().cast(),
    };
    let params = BCryptBufferDesc {
        ulVersion: NCRYPTBUFFER_VERSION,
        cBuffers: 1,
        pBuffers: &raw mut name_buf,
    };
    let mut key: NCRYPT_KEY_HANDLE = 0;
    // a named PKCS #8 import persists the key, it is not exportable
    hresult(unsafe {
        NCryptImportKey(
            prov.0,
            0,
            NCRYPT_PKCS8_PRIVATE_KEY_BLOB,
            &raw const params,
            &raw mut key,
            pkcs8.0.as_ptr(),
            u32::try_from(pkcs8.0.len()).expect("key blob fits u32"),
            NCRYPT_SILENT_FLAG,
        )
    })?;
    unsafe { NCryptFreeObject(key) };

    let info = CRYPT_KEY_PROV_INFO {
        pwszContainerName: name.as_ptr().cast_mut(),
        pwszProvName: MS_KEY_STORAGE_PROVIDER.cast_mut(),
        dwProvType: 0,
        dwFlags: 0,
        cProvParam: 0,
        rgProvParam: ptr::null_mut(),
        dwKeySpec: 0,
    };
    let ok = unsafe {
        // Schannel would use the in-memory key, drop it
        CertSetCertificateContextProperty(cert, CERT_NCRYPT_KEY_HANDLE_PROP_ID, 0, ptr::null());
        CertSetCertificateContextProperty(cert, CERT_KEY_CONTEXT_PROP_ID, 0, ptr::null());
        CertSetCertificateContextProperty(
            cert,
            CERT_KEY_PROV_INFO_PROP_ID,
            0,
            (&raw const info).cast(),
        )
    };
    if ok == 0 {
        let err = io::Error::last_os_error();
        unsafe {
            if NCryptOpenKey(prov.0, &raw mut key, name.as_ptr(), 0, NCRYPT_SILENT_FLAG) == 0 {
                NCryptDeleteKey(key, 0);
            }
        }
        return Err(err);
    }
    Ok(())
}

/// Exports the in-memory private key of a certificate as PKCS #8.
fn export_key(cert: *const CERT_CONTEXT) -> io::Result<SecretBuf> {
    // the handle is owned by the certificate
    let mut key: NCRYPT_KEY_HANDLE = 0;
    let mut len = u32::try_from(std::mem::size_of::<NCRYPT_KEY_HANDLE>()).expect("fits u32");
    if unsafe {
        CertGetCertificateContextProperty(
            cert,
            CERT_NCRYPT_KEY_HANDLE_PROP_ID,
            (&raw mut key).cast(),
            &raw mut len,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // the key is exportable, allow a plain PKCS #8 export of this in-memory copy
    let policy = NCRYPT_ALLOW_EXPORT_FLAG | NCRYPT_ALLOW_PLAINTEXT_EXPORT_FLAG;
    hresult(unsafe {
        NCryptSetProperty(
            key,
            NCRYPT_EXPORT_POLICY_PROPERTY,
            (&raw const policy).cast(),
            4,
            NCRYPT_SILENT_FLAG,
        )
    })?;

    hresult(unsafe {
        NCryptExportKey(
            key,
            0,
            NCRYPT_PKCS8_PRIVATE_KEY_BLOB,
            ptr::null(),
            ptr::null_mut(),
            0,
            &raw mut len,
            NCRYPT_SILENT_FLAG,
        )
    })?;
    let mut buf = SecretBuf(vec![0; len as usize]);
    hresult(unsafe {
        NCryptExportKey(
            key,
            0,
            NCRYPT_PKCS8_PRIVATE_KEY_BLOB,
            ptr::null(),
            buf.0.as_mut_ptr(),
            len,
            &raw mut len,
            NCRYPT_SILENT_FLAG,
        )
    })?;
    buf.0.truncate(len as usize);
    Ok(buf)
}

fn hresult(status: windows_sys::core::HRESULT) -> io::Result<()> {
    if status == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(status))
    }
}

/// Key material, zeroed on drop.
struct SecretBuf(Vec<u8>);

impl Drop for SecretBuf {
    fn drop(&mut self) {
        for b in &mut self.0 {
            unsafe { ptr::write_volatile(b, 0) };
        }
    }
}

struct Provider(NCRYPT_PROV_HANDLE);

impl Provider {
    fn open(name: windows_sys::core::PCWSTR) -> io::Result<Self> {
        let mut prov: NCRYPT_PROV_HANDLE = 0;
        hresult(unsafe { NCryptOpenStorageProvider(&raw mut prov, name, 0) })?;
        Ok(Self(prov))
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        unsafe { NCryptFreeObject(self.0) };
    }
}

/// Deletes the persisted CNG private key of a certificate, best effort.
fn delete_key(cert: *const CERT_CONTEXT) {
    let Some(info) = key_prov_info(cert) else {
        return;
    };
    let info = unsafe { &*info.as_ptr().cast::<CRYPT_KEY_PROV_INFO>() };
    // keys imported with PKCS12_ALWAYS_CNG_KSP have no legacy provider type
    if info.dwProvType != 0 || info.pwszContainerName.is_null() {
        return;
    }
    let Ok(prov) = Provider::open(info.pwszProvName) else {
        return;
    };
    let mut key: NCRYPT_KEY_HANDLE = 0;
    let flags = info.dwFlags & NCRYPT_MACHINE_KEY_FLAG | NCRYPT_SILENT_FLAG;
    unsafe {
        if NCryptOpenKey(prov.0, &raw mut key, info.pwszContainerName, 0, flags) == 0 {
            // frees the key handle
            NCryptDeleteKey(key, 0);
        }
    }
}

/// `CRYPT_KEY_PROV_INFO` of a certificate, in an aligned buffer.
fn key_prov_info(cert: *const CERT_CONTEXT) -> Option<Vec<u64>> {
    let mut len = 0u32;
    if unsafe {
        CertGetCertificateContextProperty(
            cert,
            CERT_KEY_PROV_INFO_PROP_ID,
            ptr::null_mut(),
            &raw mut len,
        )
    } == 0
    {
        return None;
    }
    let mut buf = vec![0u64; (len as usize).div_ceil(8)];
    let ok = unsafe {
        CertGetCertificateContextProperty(
            cert,
            CERT_KEY_PROV_INFO_PROP_ID,
            buf.as_mut_ptr().cast(),
            &raw mut len,
        )
    } != 0;
    (ok && len as usize >= std::mem::size_of::<CRYPT_KEY_PROV_INFO>()).then_some(buf)
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

    const PFX: &[u8] = include_bytes!("../../examples/identity.pfx");

    fn key_name(cert: &Certificate) -> Vec<u16> {
        let buf = key_prov_info(cert.as_ptr()).unwrap();
        let info = unsafe { &*buf.as_ptr().cast::<CRYPT_KEY_PROV_INFO>() };
        let name = info.pwszContainerName;
        let mut len = 0;
        while unsafe { *name.add(len) } != 0 {
            len += 1;
        }
        unsafe { std::slice::from_raw_parts(name, len + 1) }.to_vec()
    }

    fn key_exists(name: &[u16]) -> bool {
        let prov = Provider::open(MS_KEY_STORAGE_PROVIDER).unwrap();
        let mut key: NCRYPT_KEY_HANDLE = 0;
        let status =
            unsafe { NCryptOpenKey(prov.0, &raw mut key, name.as_ptr(), 0, NCRYPT_SILENT_FLAG) };
        if status == 0 {
            unsafe { NCryptFreeObject(key) };
        }
        status == 0
    }

    /// Imported keys are persisted under unique names and deleted with the
    /// certificate.
    #[test]
    fn test_from_pkcs12() {
        let cert = Certificate::from_pkcs12(PFX, "ntex").unwrap();
        let other = Certificate::from_pkcs12(PFX, "ntex").unwrap();
        assert!(format!("{cert:?}").contains("Certificate"));
        assert_eq!(cert.der(), other.der());
        assert!(has_private_key(cert.as_ptr()));

        let name = key_name(&cert);
        let other_name = key_name(&other);
        assert!(String::from_utf16_lossy(&name).starts_with("ntex-tls-"));
        assert_ne!(name, other_name);
        assert!(key_exists(&name));

        let clone = cert.clone();
        drop(cert);
        assert!(key_exists(&name));
        drop(clone);
        assert!(!key_exists(&name));
        assert!(key_exists(&other_name));
        drop(other);
        assert!(!key_exists(&other_name));
    }

    #[test]
    fn test_from_pkcs12_errors() {
        assert!(Certificate::from_pkcs12(PFX, "wrong").is_err());
        assert!(Certificate::from_pkcs12(b"not a pfx", "ntex").is_err());
    }

    #[test]
    fn test_from_store_not_found() {
        let err =
            Certificate::from_store(CertStoreLocation::CurrentUser, "MY", &[0; 20]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn test_from_store_by_subject_not_found() {
        let err = Certificate::from_store_by_subject(
            CertStoreLocation::CurrentUser,
            "MY",
            "CN=ntex no such subject 3f0c",
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);

        let err = Certificate::from_store_by_subject(CertStoreLocation::CurrentUser, "MY", "")
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
}
