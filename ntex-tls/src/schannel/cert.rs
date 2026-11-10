//! Certificate-store access and ownership of Schannel identities and imported keys.
//!
//! PFX imports use isolated persisted CNG keys for Schannel compatibility.
//! Store lookups borrow existing identities without deleting their private keys.
use std::{fmt, fmt::Write, io, ptr, sync::Arc};

use windows_sys::Win32::Security::Cryptography::{
    BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptBuffer, BCryptBufferDesc, BCryptGenRandom, CERT_CONTEXT,
    CERT_KEY_CONTEXT_PROP_ID, CERT_KEY_PROV_INFO_PROP_ID, CERT_NCRYPT_KEY_HANDLE_PROP_ID,
    CERT_SYSTEM_STORE_CURRENT_USER, CERT_SYSTEM_STORE_LOCAL_MACHINE, CRYPT_EXPORTABLE,
    CRYPT_KEY_PROV_INFO, CertGetCertificateContextProperty, CertSetCertificateContextProperty,
    MS_KEY_STORAGE_PROVIDER, NCRYPT_ALLOW_EXPORT_FLAG, NCRYPT_ALLOW_PLAINTEXT_EXPORT_FLAG,
    NCRYPT_EXPORT_POLICY_PROPERTY, NCRYPT_KEY_HANDLE, NCRYPT_MACHINE_KEY_FLAG,
    NCRYPT_PKCS8_PRIVATE_KEY_BLOB, NCRYPT_SILENT_FLAG, NCRYPTBUFFER_PKCS_KEY_NAME,
    NCRYPTBUFFER_VERSION, NCryptDeleteKey, NCryptExportKey, NCryptFreeObject, NCryptImportKey,
    NCryptOpenKey, NCryptSetProperty, PKCS12_ALWAYS_CNG_KSP, PKCS12_NO_PERSIST_KEY,
};
use zeroize::Zeroizing;

use crate::win::{self, Cert, CertStore, NcryptHandle, wide};

/// Certificate with its private key.
///
/// Identifies a server, see [`ServerConfig`](super::ServerConfig), or a
/// client, see [`ClientConfig::set_client_cert()`](super::ClientConfig::set_client_cert).
///
/// Clones share the certificate and keep its associated store alive. Keys
/// imported with [`Self::from_pkcs12`] are cleaned up after the final owner drops;
/// keys loaded from system stores remain owned by those stores.
///
/// Loading an identity does not validate its certificate chain or guarantee that
/// the operating system will permit its private key to be used in a handshake.
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
    /// current user's key store under a unique name. Deletion is attempted when
    /// the certificate and all configurations using it are dropped. The key is
    /// left behind if cleanup fails or the process does not exit normally.
    /// Key deletion and handle-release failures are logged.
    ///
    /// If the archive holds multiple certificates, the first one with a
    /// private key is used, the others are available to build its chain.
    ///
    /// # Errors
    ///
    /// Fails if the archive cannot be decoded with `password`, has no
    /// certificate with a private key, or the key cannot be stored.
    pub fn from_pkcs12(der: &[u8], password: &str) -> io::Result<Self> {
        // a key persisted by the import is named after the archive's friendly
        // name, imports of the same archive would share and delete it; the key
        // is imported to memory and persisted under a unique name instead
        let flags = PKCS12_NO_PERSIST_KEY | PKCS12_ALWAYS_CNG_KSP | CRYPT_EXPORTABLE;
        let store = CertStore::from_pfx(der, Some(password), flags)?;
        let (cert, ()) = store
            .find_map(|cert| has_private_key(cert).then_some(()))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "PKCS #12 archive has no certificate with a private key",
                )
            })?;
        let mut cert = CertContext::new(cert);
        persist_key(&cert.cert)?;
        cert.temp_key = true;
        Ok(Self(Arc::new(cert)))
    }

    /// Loads a certificate by its SHA-1 thumbprint from a system store,
    /// for example `"MY"` (Personal).
    ///
    /// The existing store is opened read-only; a missing store is not created.
    /// The thumbprint is the raw 20-byte digest, not a hexadecimal string.
    /// Unlike [`Self::from_store_by_subject`], selection does not filter by
    /// certificate validity dates. No chain trust validation is performed.
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
        let store = open_store(location, store)?;
        let cert = store
            .find_sha1(thumbprint)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "certificate is not found"))?;
        if has_private_key(&cert) {
            Ok(Self(Arc::new(CertContext::new(cert))))
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
    /// `subject` matches any part of the subject's attribute values joined by
    /// `", "`, case-insensitively, for example `"client.example.com"`.
    /// Attribute names are not part of the matched string, `"CN=client.example.com"`
    /// does not match.
    /// Only currently valid certificates with a private key are considered,
    /// the one that expires last is used.
    /// The existing store is opened read-only and no chain trust validation is
    /// performed.
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
        let store = open_store(location, store)?;

        let mut found: Option<(u64, win::CertContext)> = None;
        store.for_each_subject(subject, |cert| {
            if !cert.is_time_valid() || !has_private_key(cert) {
                return;
            }
            let not_after = cert.not_after();
            if found.as_ref().is_none_or(|(last, _)| not_after > *last) {
                found = Some((not_after, cert.duplicate()));
            }
        });
        found
            .map(|(_, cert)| Self(Arc::new(CertContext::new(cert))))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "no valid certificate with a private key matches the subject",
                )
            })
    }

    /// Borrows the certificate's DER encoding without its private key.
    ///
    /// The returned storage is owned by this certificate context and remains
    /// valid for the lifetime of the borrow.
    #[must_use]
    pub fn der(&self) -> &[u8] {
        self.0.cert.der()
    }

    /// Borrows the context pointer without transferring a Windows reference.
    ///
    /// Callers must keep an owning certificate alive while using the pointer.
    pub(crate) fn as_ptr(&self) -> *const CERT_CONTEXT {
        self.0.cert.as_ptr()
    }

    /// Borrows the certificate context.
    pub(crate) fn context(&self) -> &Cert {
        &self.0.cert
    }

    /// Takes ownership of a certificate context.
    #[cfg(all(test, feature = "openssl"))]
    pub(super) fn from_context(cert: win::CertContext) -> Self {
        Self(Arc::new(CertContext::new(cert)))
    }

    /// Imports a previously matched PKCS#8 key and leaf-first certificates for Schannel.
    ///
    /// The caller checks certificate/key correspondence before this creates a persisted key.
    pub(crate) fn from_pkcs8(
        certificates: &[crate::x509::Certificate],
        key: &[u8],
    ) -> io::Result<Self> {
        let (leaf, chain) = certificates
            .split_first()
            .ok_or_else(|| io::Error::other("certificate is missing"))?;
        let store = CertStore::memory()?;
        let mut cert = CertContext::new(store.add_encoded(leaf.der())?);
        for extra in chain {
            store.add_encoded_only(extra.der())?;
        }
        persist_pkcs8(&cert.cert, key)?;
        cert.temp_key = true;
        Ok(Self(Arc::new(cert)))
    }
}

impl fmt::Debug for Certificate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Certificate").finish_non_exhaustive()
    }
}

/// Owned certificate context, the context keeps its store alive.
struct CertContext {
    cert: win::CertContext,
    /// The private key was imported for this context and is deleted with it
    temp_key: bool,
}

impl CertContext {
    /// Existing private keys are preserved unless the importer explicitly marks this as a
    /// temporary key.
    fn new(cert: win::CertContext) -> Self {
        Self {
            cert,
            temp_key: false,
        }
    }
}

impl Drop for CertContext {
    fn drop(&mut self) {
        // the context reference is released after the key is deleted
        if self.temp_key {
            delete_key(&self.cert);
        }
    }
}

/// Persists the in-memory private key of a certificate under a unique name,
/// the certificate refers to the persisted key afterwards.
///
/// `cert` must have an exportable in-memory CNG key. The temporary export is
/// zeroed on drop; if attaching the persisted key fails, deletion of the new key
/// is attempted before returning the original error.
fn persist_key(cert: &Cert) -> io::Result<()> {
    let pkcs8 = export_key(cert)?;
    persist_pkcs8(cert, &pkcs8)
}

/// Persists supplied PKCS#8 bytes under an isolated name and attaches the key to `cert`.
fn persist_pkcs8(cert: &Cert, pkcs8: &[u8]) -> io::Result<()> {
    let mut id = [0u8; 16];
    // SAFETY: `id` is writable for 16 bytes; no algorithm handle with the system RNG flag.
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

    let prov = NcryptHandle::software_provider().map_err(io::Error::from_raw_os_error)?;
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
    // SAFETY: the provider is open, `params` and its name buffer outlive the call, `pkcs8`
    // is readable for its length.
    hresult(unsafe {
        NCryptImportKey(
            prov.as_raw(),
            0,
            NCRYPT_PKCS8_PRIVATE_KEY_BLOB,
            &raw const params,
            &raw mut key,
            pkcs8.as_ptr(),
            u32::try_from(pkcs8.len()).map_err(|_| io::Error::other("private key is too large"))?,
            NCRYPT_SILENT_FLAG,
        )
    })?;
    // SAFETY: a successful import returns an owned handle; the key stays persisted.
    drop(unsafe { NcryptHandle::from_raw(key) });

    let info = CRYPT_KEY_PROV_INFO {
        pwszContainerName: name.as_ptr().cast_mut(),
        pwszProvName: MS_KEY_STORAGE_PROVIDER.cast_mut(),
        dwProvType: 0,
        dwFlags: 0,
        cProvParam: 0,
        rgProvParam: ptr::null_mut(),
        dwKeySpec: 0,
    };
    let cert = cert.as_ptr();
    // SAFETY: the context is live; null data removes a property, `info` and the strings it
    // points to outlive the call (the property is copied).
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
        // SAFETY: the provider is open and the name is NUL terminated; an opened key is owned.
        if unsafe {
            NCryptOpenKey(
                prov.as_raw(),
                &raw mut key,
                name.as_ptr(),
                0,
                NCRYPT_SILENT_FLAG,
            )
        } == 0
        {
            delete_owned_key(key);
        }
        return Err(err);
    }
    Ok(())
}

/// Exports the in-memory private key of a certificate as PKCS #8.
///
/// The CNG handle is borrowed from `cert`, not released, and the exported
/// plaintext bytes are owned by a zero-on-drop buffer.
fn export_key(cert: &Cert) -> io::Result<Zeroizing<Vec<u8>>> {
    // the handle is owned by the certificate
    let mut key: NCRYPT_KEY_HANDLE = 0;
    let mut len = u32::try_from(std::mem::size_of::<NCRYPT_KEY_HANDLE>()).expect("fits u32");
    // SAFETY: the context is live, `key` is writable for `len` bytes.
    if unsafe {
        CertGetCertificateContextProperty(
            cert.as_ptr(),
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
    // SAFETY: the key is kept alive by `cert`, the policy is a readable `u32`.
    hresult(unsafe {
        NCryptSetProperty(
            key,
            NCRYPT_EXPORT_POLICY_PROPERTY,
            (&raw const policy).cast(),
            4,
            NCRYPT_SILENT_FLAG,
        )
    })?;

    // SAFETY: the key is live; null output queries the size.
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
    let mut buf = Zeroizing::new(vec![0; len as usize]);
    // SAFETY: the key is live, `buf` is writable for `len` bytes.
    hresult(unsafe {
        NCryptExportKey(
            key,
            0,
            NCRYPT_PKCS8_PRIVATE_KEY_BLOB,
            ptr::null(),
            buf.as_mut_ptr(),
            len,
            &raw mut len,
            NCRYPT_SILENT_FLAG,
        )
    })?;
    buf.truncate(len as usize);
    Ok(buf)
}

/// Converts the CNG APIs' zero-success status convention to an I/O result.
fn hresult(status: windows_sys::core::HRESULT) -> io::Result<()> {
    if status == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(status))
    }
}

/// Deletes the persisted CNG private key of a certificate, best effort.
///
/// `cert` must refer to an importer-owned temporary key. Legacy providers and
/// missing key metadata are ignored; system-store keys must not be passed here.
fn delete_key(cert: &Cert) {
    let Some(info) = key_prov_info(cert) else {
        return;
    };
    // SAFETY: `key_prov_info` returns an aligned buffer holding a `CRYPT_KEY_PROV_INFO`.
    let info = unsafe { &*info.as_ptr().cast::<CRYPT_KEY_PROV_INFO>() };
    // keys imported with PKCS12_ALWAYS_CNG_KSP have no legacy provider type
    if info.dwProvType != 0 || info.pwszContainerName.is_null() {
        return;
    }
    // SAFETY: the provider name points into `info`, which is live.
    let Ok(prov) = (unsafe { NcryptHandle::open_provider(info.pwszProvName) }) else {
        return;
    };
    let mut key: NCRYPT_KEY_HANDLE = 0;
    let flags = info.dwFlags & NCRYPT_MACHINE_KEY_FLAG | NCRYPT_SILENT_FLAG;
    // SAFETY: the provider is open, the container name points into `info`; an opened key is
    // owned.
    if unsafe {
        NCryptOpenKey(
            prov.as_raw(),
            &raw mut key,
            info.pwszContainerName,
            0,
            flags,
        )
    } == 0
    {
        delete_owned_key(key);
    }
}

/// Consumes an owned key handle, deleting its persisted key or releasing it on failure.
fn delete_owned_key(key: NCRYPT_KEY_HANDLE) {
    delete_owned_key_with(
        key,
        // SAFETY: the handle is owned, deletion frees it on success.
        |key| unsafe { NCryptDeleteKey(key, 0) },
        // SAFETY: called only if deletion failed, the handle is still owned.
        |key| unsafe { NCryptFreeObject(key) },
    );
}

/// Implements key deletion with injectable Windows operations.
///
/// Successful deletion also frees the handle. On failure, releases the handle
/// before logging because application logging callbacks can unwind.
fn delete_owned_key_with(
    key: NCRYPT_KEY_HANDLE,
    delete: impl FnOnce(NCRYPT_KEY_HANDLE) -> windows_sys::core::HRESULT,
    free: impl FnOnce(NCRYPT_KEY_HANDLE) -> windows_sys::core::HRESULT,
) {
    if let Err(err) = hresult(delete(key)) {
        // Successful deletion frees the handle; on failure release it before
        // logging, which may unwind through application code.
        let released = hresult(free(key));
        log::error!("Failed to delete persisted Schannel private key: {err}");
        if let Err(err) = released {
            log::error!("Failed to release Schannel private key handle: {err}");
        }
    }
}

/// `CRYPT_KEY_PROV_INFO` of a certificate, in an aligned buffer.
///
/// Embedded string pointers refer into the returned buffer and must not outlive
/// or be used after moving its contents to another allocation.
fn key_prov_info(cert: &Cert) -> Option<Vec<u64>> {
    let cert = cert.as_ptr();
    let mut len = 0u32;
    // SAFETY: the context is live; null output queries the size.
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
    // SAFETY: `buf` is writable for at least `len` bytes and 8-byte aligned.
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

/// Opens an existing system store read-only, never creating a missing store.
fn open_store(location: CertStoreLocation, name: &str) -> io::Result<CertStore> {
    let location = match location {
        CertStoreLocation::CurrentUser => CERT_SYSTEM_STORE_CURRENT_USER,
        CertStoreLocation::LocalMachine => CERT_SYSTEM_STORE_LOCAL_MACHINE,
    };
    CertStore::open_system(location, name)
}

/// Checks a certificate context for a key handle or provider association.
///
/// This examines properties only; it does not open the key or prove that the
/// current process has permission to use it.
fn has_private_key(cert: &Cert) -> bool {
    [
        CERT_KEY_PROV_INFO_PROP_ID,
        CERT_KEY_CONTEXT_PROP_ID,
        CERT_NCRYPT_KEY_HANDLE_PROP_ID,
    ]
    .into_iter()
    .any(|prop| {
        let mut len = 0u32;
        // SAFETY: the context is live; null output queries the size.
        unsafe {
            CertGetCertificateContextProperty(cert.as_ptr(), prop, ptr::null_mut(), &raw mut len)
                != 0
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PFX: &[u8] = include_bytes!("../../examples/identity.pfx");

    /// STATEMENT: Owned-key cleanup frees the handle only when deleting the key fails.
    ///
    /// HOW:
    /// - Inject successful and failing delete/free callbacks and record their calls.
    /// - Check successful deletion stands alone and failed deletion is followed by one free.
    #[test]
    fn test_delete_owned_key() {
        use std::cell::RefCell;
        use windows_sys::Win32::Foundation::{NTE_INVALID_HANDLE, NTE_PERM};

        for (delete_status, free_status) in [(0, 0), (NTE_PERM, 0), (NTE_PERM, NTE_INVALID_HANDLE)]
        {
            let calls = RefCell::new(Vec::new());
            delete_owned_key_with(
                42,
                |key| {
                    calls.borrow_mut().push(("delete", key));
                    delete_status
                },
                |key| {
                    calls.borrow_mut().push(("free", key));
                    free_status
                },
            );
            let expected = if delete_status == 0 {
                vec![("delete", 42)]
            } else {
                vec![("delete", 42), ("free", 42)]
            };
            assert_eq!(calls.into_inner(), expected);
        }
    }

    fn key_name(cert: &Certificate) -> Vec<u16> {
        let buf = key_prov_info(cert.context()).unwrap();
        let info = unsafe { &*buf.as_ptr().cast::<CRYPT_KEY_PROV_INFO>() };
        let name = info.pwszContainerName;
        let mut len = 0;
        while unsafe { *name.add(len) } != 0 {
            len += 1;
        }
        unsafe { std::slice::from_raw_parts(name, len + 1) }.to_vec()
    }

    fn key_exists(name: &[u16]) -> bool {
        let prov = NcryptHandle::software_provider().unwrap();
        let mut key: NCRYPT_KEY_HANDLE = 0;
        let status = unsafe {
            NCryptOpenKey(
                prov.as_raw(),
                &raw mut key,
                name.as_ptr(),
                0,
                NCRYPT_SILENT_FLAG,
            )
        };
        if status == 0 {
            drop(unsafe { NcryptHandle::from_raw(key) });
        }
        status == 0
    }

    /// STATEMENT: Imported keys have unique names and remain persisted until the last certificate clone drops.
    ///
    /// HOW:
    /// - Import the same PKCS#12 twice and check matching DER, private-key presence, and distinct key names.
    /// - Drop an original and its clone separately, checking that only the last drop deletes its key.
    /// - Check the second import retains its independent key until it too is dropped.
    #[test]
    fn test_from_pkcs12() {
        let cert = Certificate::from_pkcs12(PFX, "ntex").unwrap();
        let other = Certificate::from_pkcs12(PFX, "ntex").unwrap();
        assert!(format!("{cert:?}").contains("Certificate"));
        assert_eq!(cert.der(), other.der());
        assert!(has_private_key(cert.context()));

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

    /// STATEMENT: PKCS#12 import rejects incorrect passwords and malformed input.
    ///
    /// HOW:
    /// - Import the fixture with a wrong password and non-PKCS#12 bytes with the correct password.
    /// - Assert both imports return errors.
    #[test]
    fn test_from_pkcs12_errors() {
        assert!(Certificate::from_pkcs12(PFX, "wrong").is_err());
        assert!(Certificate::from_pkcs12(b"not a pfx", "ntex").is_err());
    }

    /// STATEMENT: PKCS#12 import requires a private key alongside the certificate.
    ///
    /// HOW:
    /// - Use OpenSSL to build a certificate-only PKCS#12 archive.
    /// - Import it and check for `InvalidInput` with a private-key diagnostic.
    #[cfg(feature = "openssl")]
    #[test]
    fn test_from_pkcs12_no_private_key() {
        let cert =
            tls_openssl::x509::X509::from_pem(include_bytes!("../../examples/cert.pem")).unwrap();
        let pfx = tls_openssl::pkcs12::Pkcs12::builder()
            .cert(&cert)
            .build2("ntex")
            .unwrap()
            .to_der()
            .unwrap();
        let err = Certificate::from_pkcs12(&pfx, "ntex").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("private key"), "{err}");
    }

    /// STATEMENT: Certificate lookup fails without creating a missing certificate store.
    ///
    /// HOW:
    /// - Look up a certificate by thumbprint and subject in a nonexistent current-user store.
    /// - Check both lookups fail and a subsequent attempt to open the store also fails.
    #[test]
    fn test_from_store_missing_store() {
        let name = "ntex-no-such-store-3f0c";
        assert!(Certificate::from_store(CertStoreLocation::CurrentUser, name, &[0; 20]).is_err());
        assert!(
            Certificate::from_store_by_subject(CertStoreLocation::CurrentUser, name, "CN=ntex")
                .is_err()
        );
        // the store is not created
        assert!(open_store(CertStoreLocation::CurrentUser, name).is_err());
    }

    /// STATEMENT: Store lookup rejects a certificate without a private key.
    ///
    /// HOW:
    /// - Find a current-user root certificate without a private key and read its thumbprint.
    /// - Look it up through `Certificate::from_store` and check for `InvalidInput`.
    #[test]
    fn test_from_store_no_private_key() {
        use windows_sys::Win32::Security::Cryptography::CERT_SHA1_HASH_PROP_ID;

        // trusted root certificates have no private keys, usually
        let store = open_store(CertStoreLocation::CurrentUser, "Root").unwrap();
        let (_, thumbprint) = store
            .find_map(|cert| {
                if has_private_key(cert) {
                    return None;
                }
                let mut thumbprint = [0u8; 20];
                let mut len = 20u32;
                let ok = unsafe {
                    CertGetCertificateContextProperty(
                        cert.as_ptr(),
                        CERT_SHA1_HASH_PROP_ID,
                        thumbprint.as_mut_ptr().cast(),
                        &raw mut len,
                    )
                };
                assert!(ok != 0 && len == 20);
                Some(thumbprint)
            })
            .expect("no root certificate without a private key");

        let err = Certificate::from_store(CertStoreLocation::CurrentUser, "Root", &thumbprint)
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    /// STATEMENT: A missing certificate thumbprint is reported as `NotFound`.
    ///
    /// HOW:
    /// - Query the current user's `MY` store with an all-zero thumbprint.
    /// - Check the lookup error kind is `NotFound`.
    #[test]
    fn test_from_store_not_found() {
        let err =
            Certificate::from_store(CertStoreLocation::CurrentUser, "MY", &[0; 20]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    /// STATEMENT: Subject lookup distinguishes a missing certificate from an invalid empty subject.
    ///
    /// HOW:
    /// - Query the current user's `MY` store with a nonexistent subject and check for `NotFound`.
    /// - Query it with an empty subject and check for `InvalidInput`.
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
