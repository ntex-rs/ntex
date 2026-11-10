//! Owned Crypt32 and CNG handles shared by the Schannel and X.509 backends.
//!
//! Every handle is released on drop. Borrowed certificate contexts are exposed as `&Cert`,
//! tied to the lifetime of their owner (an owned context, a chain or a store enumeration).
use std::{io, ptr, ptr::NonNull};

use windows_sys::Win32::Foundation::FILETIME;
use windows_sys::Win32::Security::Cryptography::{
    CERT_CHAIN_CONTEXT, CERT_CHAIN_PARA, CERT_CONTEXT, CERT_FIND_SHA1_HASH,
    CERT_FIND_SUBJECT_STR_W, CERT_INFO, CERT_STORE_ADD_ALWAYS, CERT_STORE_CREATE_NEW_FLAG,
    CERT_STORE_OPEN_EXISTING_FLAG, CERT_STORE_PROV_MEMORY, CERT_STORE_PROV_SYSTEM_W,
    CERT_STORE_READONLY_FLAG, CRYPT_INTEGER_BLOB, CRYPT_VERIFY_CERT_SIGN_ISSUER_CERT,
    CRYPT_VERIFY_CERT_SIGN_SUBJECT_CERT, CertAddEncodedCertificateToStore, CertCloseStore,
    CertCreateCertificateContext, CertDuplicateCertificateContext, CertEnumCertificatesInStore,
    CertFindCertificateInStore, CertFreeCertificateChain, CertFreeCertificateContext,
    CertGetCertificateChain, CertOpenStore, CertVerifyTimeValidity,
    CryptVerifyCertificateSignatureEx, HCERTCHAINENGINE, HCERTSTORE, MS_KEY_STORAGE_PROVIDER,
    NCRYPT_HANDLE, NCryptFreeObject, NCryptOpenStorageProvider, PFXImportCertStore, PFXIsPFXBlob,
    PKCS_7_ASN_ENCODING, X509_ASN_ENCODING,
};
use windows_sys::core::{HRESULT, PCWSTR};

const ENCODING: u32 = X509_ASN_ENCODING | PKCS_7_ASN_ENCODING;

/// Encodes a Windows string and appends its terminating UTF-16 NUL.
pub(crate) fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Checks whether `der` looks like a PKCS #12 archive.
pub(crate) fn is_pfx(der: &[u8]) -> bool {
    let Ok(blob) = pfx_blob(der) else {
        return false;
    };
    // SAFETY: `blob` points to `der`, which outlives the call.
    unsafe { PFXIsPFXBlob(&raw const blob) != 0 }
}

fn pfx_blob(der: &[u8]) -> io::Result<CRYPT_INTEGER_BLOB> {
    Ok(CRYPT_INTEGER_BLOB {
        cbData: len32(der, "PKCS #12 archive")?,
        pbData: der.as_ptr().cast_mut(),
    })
}

fn len32(data: &[u8], what: &str) -> io::Result<u32> {
    u32::try_from(data.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, format!("{what} is too large")))
}

/// Borrows a Windows array, treating a null pointer or zero length as empty.
///
/// # Safety
/// Non-null, nonempty storage must be readable for `len` elements and remain live and
/// immutable for the returned lifetime.
unsafe fn slice<'a, T>(data: *const T, len: u32) -> &'a [T] {
    if data.is_null() || len == 0 {
        &[]
    } else {
        // SAFETY: guaranteed by the caller.
        unsafe { std::slice::from_raw_parts(data, len as usize) }
    }
}

/// Borrowed certificate context.
///
/// Only exists behind a reference whose lifetime is bounded by the owner of the context.
#[repr(transparent)]
pub(crate) struct Cert(CERT_CONTEXT);

// SAFETY: a certificate context is immutable once created, Crypt32 synchronizes its property
// list and reference count internally.
unsafe impl Sync for Cert {}

impl Cert {
    /// Borrows a live certificate context.
    ///
    /// # Safety
    /// `cert` must be a non-null, valid context that stays alive for `'a`.
    pub(crate) unsafe fn from_ptr<'a>(cert: *const CERT_CONTEXT) -> &'a Cert {
        // SAFETY: guaranteed by the caller; `Cert` is a transparent wrapper.
        unsafe { &*cert.cast::<Cert>() }
    }

    /// Context pointer, valid while `self` is borrowed.
    pub(crate) fn as_ptr(&self) -> *const CERT_CONTEXT {
        &raw const self.0
    }

    /// DER encoding of the certificate.
    pub(crate) fn der(&self) -> &[u8] {
        // SAFETY: the encoded certificate is owned by the live context.
        unsafe { slice(self.0.pbCertEncoded, self.0.cbCertEncoded) }
    }

    /// Decoded certificate fields.
    pub(crate) fn info(&self) -> &CERT_INFO {
        // SAFETY: a valid context always has decoded info owned by the context.
        unsafe { &*self.0.pCertInfo }
    }

    /// Encoded subject name.
    pub(crate) fn subject_name(&self) -> &[u8] {
        let name = &self.info().Subject;
        // SAFETY: the name is owned by the live context.
        unsafe { slice(name.pbData, name.cbData) }
    }

    /// Encoded issuer name.
    pub(crate) fn issuer_name(&self) -> &[u8] {
        let name = &self.info().Issuer;
        // SAFETY: the name is owned by the live context.
        unsafe { slice(name.pbData, name.cbData) }
    }

    /// Checks the validity period against the current time.
    pub(crate) fn is_time_valid(&self) -> bool {
        // SAFETY: null time means the current time, the info is live.
        unsafe { CertVerifyTimeValidity(ptr::null(), self.info()) == 0 }
    }

    /// End of the validity period, in `FILETIME` ticks.
    pub(crate) fn not_after(&self) -> u64 {
        let t = self.info().NotAfter;
        (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)
    }

    /// Takes a new owned reference to this context.
    pub(crate) fn duplicate(&self) -> CertContext {
        // SAFETY: the context is live; duplication only increments its reference count and
        // returns the same non-null context.
        unsafe { CertContext::from_raw(CertDuplicateCertificateContext(self.as_ptr())) }
            .expect("CertDuplicateCertificateContext returns its argument")
    }

    /// Verifies the certificate signature with the public key of `issuer`.
    pub(crate) fn is_signed_by(&self, issuer: &Cert) -> bool {
        // SAFETY: both contexts are live and match the specified subject types.
        unsafe {
            CryptVerifyCertificateSignatureEx(
                0,
                ENCODING,
                CRYPT_VERIFY_CERT_SIGN_SUBJECT_CERT,
                self.as_ptr().cast(),
                CRYPT_VERIFY_CERT_SIGN_ISSUER_CERT,
                issuer.as_ptr().cast(),
                0,
                ptr::null_mut(),
            ) != 0
        }
    }

    /// Visits every certificate of the store this context belongs to.
    pub(crate) fn for_each_in_store(&self, mut f: impl FnMut(&Cert)) {
        if !self.0.hCertStore.is_null() {
            // SAFETY: the context keeps its store open.
            unsafe {
                scan(self.0.hCertStore, Query::All, |cert| {
                    f(cert);
                    None::<()>
                })
            };
        }
    }
}

/// Owned certificate context reference, released with `CertFreeCertificateContext`.
///
/// The context keeps its store alive.
pub(crate) struct CertContext(NonNull<CERT_CONTEXT>);

// SAFETY: see `Cert`; the reference is only released on drop.
unsafe impl Send for CertContext {}
// SAFETY: see `Send`.
unsafe impl Sync for CertContext {}

impl CertContext {
    /// Takes ownership of a context reference, `None` if `cert` is null.
    ///
    /// # Safety
    /// A non-null `cert` must be a valid context reference that the caller owns and does not
    /// release afterwards.
    pub(crate) unsafe fn from_raw(cert: *const CERT_CONTEXT) -> Option<Self> {
        NonNull::new(cert.cast_mut()).map(Self)
    }

    /// Decodes a standalone certificate context.
    pub(crate) fn from_der(der: &[u8]) -> io::Result<Self> {
        let len = len32(der, "certificate")?;
        // SAFETY: `der` is readable for `len` bytes, the API copies it into an owned context.
        let cert = unsafe { CertCreateCertificateContext(X509_ASN_ENCODING, der.as_ptr(), len) };
        // SAFETY: a non-null result is an owned reference.
        unsafe { Self::from_raw(cert) }.ok_or_else(io::Error::last_os_error)
    }
}

impl std::ops::Deref for CertContext {
    type Target = Cert;

    fn deref(&self) -> &Cert {
        // SAFETY: the owned context lives as long as `self`.
        unsafe { Cert::from_ptr(self.0.as_ptr()) }
    }
}

impl Clone for CertContext {
    fn clone(&self) -> Self {
        self.duplicate()
    }
}

impl Drop for CertContext {
    fn drop(&mut self) {
        // SAFETY: the reference is owned.
        unsafe { CertFreeCertificateContext(self.0.as_ptr()) };
    }
}

/// Certificates visited by `scan`.
#[derive(Clone, Copy)]
enum Query<'a> {
    All,
    Subject(&'a [u16]),
}

/// Enumerates `store`, the first certificate `f` maps to `Some` is returned as an owned
/// reference; every other context is released by the next enumeration call.
///
/// # Safety
/// `store` must be open for the call.
unsafe fn scan<T>(
    store: HCERTSTORE,
    query: Query<'_>,
    mut f: impl FnMut(&Cert) -> Option<T>,
) -> Option<(CertContext, T)> {
    let mut cert: *const CERT_CONTEXT = ptr::null();
    loop {
        // SAFETY: the store is open, a subject is NUL terminated; the call releases the
        // previous context and returns an owned reference or null.
        cert = unsafe {
            match query {
                Query::All => CertEnumCertificatesInStore(store, cert),
                Query::Subject(subject) => CertFindCertificateInStore(
                    store,
                    ENCODING,
                    0,
                    CERT_FIND_SUBJECT_STR_W,
                    subject.as_ptr().cast(),
                    cert,
                ),
            }
        };
        if cert.is_null() {
            return None;
        }
        // SAFETY: `cert` is live until the next enumeration call, `f` cannot retain it.
        if let Some(value) = f(unsafe { Cert::from_ptr(cert) }) {
            // SAFETY: enumeration stops, the reference is now owned here.
            return unsafe { CertContext::from_raw(cert) }.map(|cert| (cert, value));
        }
    }
}

/// Owned certificate store, closed on drop.
///
/// Open certificate contexts keep the store alive, it is closed without forcing.
pub(crate) struct CertStore(NonNull<std::ffi::c_void>);

// SAFETY: certificate stores are internally synchronized, the handle is only closed on drop.
unsafe impl Send for CertStore {}
// SAFETY: see `Send`.
unsafe impl Sync for CertStore {}

impl CertStore {
    fn from_handle(store: HCERTSTORE) -> io::Result<Self> {
        NonNull::new(store)
            .map(Self)
            .ok_or_else(io::Error::last_os_error)
    }

    /// Creates an empty in-memory store.
    pub(crate) fn memory() -> io::Result<Self> {
        // SAFETY: the memory provider takes no parameters.
        Self::from_handle(unsafe {
            CertOpenStore(
                CERT_STORE_PROV_MEMORY,
                0,
                0,
                CERT_STORE_CREATE_NEW_FLAG,
                ptr::null(),
            )
        })
    }

    /// Opens an existing system store read-only, a missing store is not created.
    ///
    /// `location` is a `CERT_SYSTEM_STORE_*` location flag.
    pub(crate) fn open_system(location: u32, name: &str) -> io::Result<Self> {
        let name = wide(name);
        // SAFETY: the system provider takes a NUL terminated wide store name.
        Self::from_handle(unsafe {
            CertOpenStore(
                CERT_STORE_PROV_SYSTEM_W,
                0,
                0,
                location | CERT_STORE_READONLY_FLAG | CERT_STORE_OPEN_EXISTING_FLAG,
                name.as_ptr().cast(),
            )
        })
    }

    /// Imports a PKCS #12 archive with `PFXImportCertStore`.
    ///
    /// `None` passes a NULL password, which differs from an empty one.
    pub(crate) fn from_pfx(der: &[u8], password: Option<&str>, flags: u32) -> io::Result<Self> {
        let blob = pfx_blob(der)?;
        let password = password.map(wide);
        let password = password.as_ref().map_or(ptr::null(), Vec::as_ptr);
        // SAFETY: `blob` points to `der`, the password is NUL terminated or null.
        Self::from_handle(unsafe { PFXImportCertStore(&raw const blob, password, flags) })
    }

    /// Raw handle, valid while `self` is alive.
    pub(crate) fn as_raw(&self) -> HCERTSTORE {
        self.0.as_ptr()
    }

    fn add(&self, der: &[u8], out: *mut *mut CERT_CONTEXT) -> io::Result<()> {
        let len = len32(der, "certificate")?;
        // SAFETY: the store is open, `der` is readable for `len` bytes and `out` is a valid
        // out pointer or null.
        let ok = unsafe {
            CertAddEncodedCertificateToStore(
                self.as_raw(),
                X509_ASN_ENCODING,
                der.as_ptr(),
                len,
                CERT_STORE_ADD_ALWAYS,
                out,
            )
        };
        if ok == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// Adds a DER certificate and returns the store's context.
    pub(crate) fn add_encoded(&self, der: &[u8]) -> io::Result<CertContext> {
        let mut out = ptr::null_mut();
        self.add(der, &raw mut out)?;
        // SAFETY: a successful call returns an owned reference.
        unsafe { CertContext::from_raw(out) }.ok_or_else(|| io::Error::other("no context"))
    }

    /// Adds a DER certificate.
    pub(crate) fn add_encoded_only(&self, der: &[u8]) -> io::Result<()> {
        self.add(der, ptr::null_mut())
    }

    /// Finds a certificate by its SHA-1 thumbprint.
    pub(crate) fn find_sha1(&self, thumbprint: &[u8; 20]) -> Option<CertContext> {
        let hash = CRYPT_INTEGER_BLOB {
            cbData: 20,
            pbData: thumbprint.as_ptr().cast_mut(),
        };
        // SAFETY: the store is open, `hash` points to the thumbprint for the call; the result
        // is an owned reference or null.
        unsafe {
            CertContext::from_raw(CertFindCertificateInStore(
                self.as_raw(),
                ENCODING,
                0,
                CERT_FIND_SHA1_HASH,
                (&raw const hash).cast(),
                ptr::null(),
            ))
        }
    }

    /// Returns the first certificate `f` maps to `Some`, with the mapped value.
    pub(crate) fn find_map<T>(
        &self,
        f: impl FnMut(&Cert) -> Option<T>,
    ) -> Option<(CertContext, T)> {
        // SAFETY: the store is open.
        unsafe { scan(self.as_raw(), Query::All, f) }
    }

    /// Visits the certificates whose subject contains `subject`, see `CERT_FIND_SUBJECT_STR_W`.
    pub(crate) fn for_each_subject(&self, subject: &str, mut f: impl FnMut(&Cert)) {
        let subject = wide(subject);
        // SAFETY: the store is open.
        unsafe {
            scan(self.as_raw(), Query::Subject(&subject), |cert| {
                f(cert);
                None::<()>
            })
        };
    }
}

impl Drop for CertStore {
    fn drop(&mut self) {
        // SAFETY: the handle is owned; open contexts keep the store alive (no force flag).
        unsafe { CertCloseStore(self.as_raw(), 0) };
    }
}

/// Owned certificate chain context.
pub(crate) struct ChainContext(NonNull<CERT_CHAIN_CONTEXT>);

impl ChainContext {
    /// Builds the chain of `leaf` with `CertGetCertificateChain`.
    ///
    /// # Safety
    /// `engine` must be a live chain engine or a predefined engine handle, and every pointer
    /// in `para` must be valid for the call.
    pub(crate) unsafe fn build(
        engine: HCERTCHAINENGINE,
        leaf: &Cert,
        time: Option<&FILETIME>,
        extra: Option<&CertStore>,
        para: &CERT_CHAIN_PARA,
        flags: u32,
    ) -> io::Result<Self> {
        let mut chain: *mut CERT_CHAIN_CONTEXT = ptr::null_mut();
        // SAFETY: guaranteed by the caller for `engine` and `para`; the other pointers are
        // live for the call and `chain` receives an owned context.
        let ok = unsafe {
            CertGetCertificateChain(
                engine,
                leaf.as_ptr(),
                time.map_or(ptr::null(), ptr::from_ref),
                extra.map_or(ptr::null_mut(), CertStore::as_raw),
                para,
                flags,
                ptr::null(),
                &raw mut chain,
            )
        };
        match NonNull::new(chain) {
            Some(chain) if ok != 0 => Ok(Self(chain)),
            _ => Err(io::Error::last_os_error()),
        }
    }

    fn context(&self) -> &CERT_CHAIN_CONTEXT {
        // SAFETY: the context is owned.
        unsafe { self.0.as_ref() }
    }

    /// `CERT_TRUST_STATUS::dwErrorStatus` of the whole chain.
    pub(crate) fn error_status(&self) -> u32 {
        self.context().TrustStatus.dwErrorStatus
    }

    /// Certificates of the first simple chain, leaf first.
    pub(crate) fn certificates(&self) -> Vec<&Cert> {
        let ctx = self.context();
        // SAFETY: `rgpChain` has `cChain` valid entries, each with `cElement` valid elements
        // whose certificate contexts live as long as the chain.
        unsafe {
            let Some(&simple) = slice(ctx.rgpChain.cast_const(), ctx.cChain).first() else {
                return Vec::new();
            };
            slice((*simple).rgpElement.cast_const(), (*simple).cElement)
                .iter()
                .map(|&el| Cert::from_ptr((*el).pCertContext))
                .collect()
        }
    }
}

impl Drop for ChainContext {
    fn drop(&mut self) {
        // SAFETY: the context is owned.
        unsafe { CertFreeCertificateChain(self.0.as_ptr()) };
    }
}

/// Owned CNG provider or key handle, released with `NCryptFreeObject`.
///
/// CNG handles may be used from any thread.
pub(crate) struct NcryptHandle(NCRYPT_HANDLE);

impl NcryptHandle {
    /// Opens the Microsoft software key storage provider.
    ///
    /// # Errors
    /// The failed `SECURITY_STATUS`.
    pub(crate) fn software_provider() -> Result<Self, HRESULT> {
        // SAFETY: the provider name is a static NUL terminated string.
        unsafe { Self::open_provider(MS_KEY_STORAGE_PROVIDER) }
    }

    /// Opens a key storage provider.
    ///
    /// # Errors
    /// The failed `SECURITY_STATUS`.
    ///
    /// # Safety
    /// `name` must be a NUL terminated wide string, live for the call.
    pub(crate) unsafe fn open_provider(name: PCWSTR) -> Result<Self, HRESULT> {
        let mut handle = 0;
        // SAFETY: guaranteed by the caller for `name`, the out pointer is valid.
        let status = unsafe { NCryptOpenStorageProvider(&raw mut handle, name, 0) };
        if status == 0 { Ok(Self(handle)) } else { Err(status) }
    }

    /// Takes ownership of a handle.
    ///
    /// # Safety
    /// `handle` must be a valid handle that the caller owns and does not release afterwards.
    pub(crate) unsafe fn from_raw(handle: NCRYPT_HANDLE) -> Self {
        Self(handle)
    }

    /// Raw handle, valid while `self` is alive.
    pub(crate) fn as_raw(&self) -> NCRYPT_HANDLE {
        self.0
    }
}

impl Drop for NcryptHandle {
    fn drop(&mut self) {
        // SAFETY: the handle is owned.
        unsafe { NCryptFreeObject(self.0) };
    }
}
