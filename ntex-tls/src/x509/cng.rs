//! CNG client identity (Windows).
//!
//! The private key is never exported: it stays in its CNG key storage provider (software KSP,
//! TPM, ...) and every TLS handshake signature is made in place with `NCryptSignHash`. The
//! message is hashed with `SymCrypt`, so no OpenSSL is involved on Windows.
//!
//! Loading:
//! - PKCS#12: `PFXImportCertStore` with `PKCS12_NO_PERSIST_KEY | PKCS12_ALWAYS_CNG_KSP`, the key
//!   lives in memory only (nothing is written to the user key store), the first certificate
//!   with a usable private key is the leaf;
//! - system store: same selection rules as ntex-tls schannel (see [`crate::x509::identity`]), a
//!   certificate is only selected if its private key can be acquired silently.
//!
//! The chain is built by the Windows chain engine (local machine engine for `LocalMachine`
//! stores, current user otherwise) from the archive certificates and the system stores, with
//! no network access. Trust errors are ignored: this is the chain we present, not a chain we
//! verify. A self-signed root is not sent.
use std::{fmt, mem, ptr, sync::Arc};

use windows_sys::Win32::Security::Cryptography::{
    BCRYPT_PKCS1_PADDING_INFO, BCRYPT_PSS_PADDING_INFO, BCRYPT_SHA256_ALGORITHM,
    BCRYPT_SHA384_ALGORITHM, BCRYPT_SHA512_ALGORITHM, CERT_CHAIN_CACHE_ONLY_URL_RETRIEVAL,
    CERT_CHAIN_CONTEXT, CERT_CHAIN_DISABLE_AIA, CERT_CHAIN_PARA, CERT_CONTEXT, CERT_FIND_SHA1_HASH,
    CERT_FIND_SUBJECT_STR_W, CERT_KEY_SPEC, CERT_NCRYPT_KEY_SPEC, CERT_STORE_OPEN_EXISTING_FLAG,
    CERT_STORE_PROV_SYSTEM_W, CERT_STORE_READONLY_FLAG, CERT_SYSTEM_STORE_CURRENT_USER,
    CERT_SYSTEM_STORE_LOCAL_MACHINE, CRYPT_ACQUIRE_ONLY_NCRYPT_KEY_FLAG, CRYPT_ACQUIRE_SILENT_FLAG,
    CRYPT_INTEGER_BLOB, CRYPT_VERIFY_CERT_SIGN_ISSUER_CERT, CRYPT_VERIFY_CERT_SIGN_SUBJECT_CERT,
    CertCloseStore, CertDuplicateCertificateContext, CertEnumCertificatesInStore,
    CertFindCertificateInStore, CertFreeCertificateChain, CertFreeCertificateContext,
    CertGetCertificateChain, CertOpenStore, CertVerifyTimeValidity,
    CryptVerifyCertificateSignatureEx, HCERTCHAINENGINE, HCERTSTORE, NCRYPT_KEY_HANDLE,
    NCRYPT_PAD_PKCS1_FLAG, NCRYPT_PAD_PSS_FLAG, NCRYPT_SILENT_FLAG, NCryptFreeObject,
    NCryptSignHash, PFXImportCertStore, PKCS_7_ASN_ENCODING, PKCS12_ALWAYS_CNG_KSP,
    PKCS12_NO_PERSIST_KEY, X509_ASN_ENCODING,
};

use crate::x509::identity::{StoreLocation, StoreSelector};
use crate::x509::key::{HashAlg, SignScheme};
use crate::x509::{Certificate, Error, KeyType};

/// `HCCE_LOCAL_MACHINE` (not exported by windows-sys).
const HCCE_LOCAL_MACHINE: HCERTCHAINENGINE = 1 as HCERTCHAINENGINE;

/// Private key of a [`ClientIdentity`](crate::x509::identity::ClientIdentity), kept in CNG.
///
/// Clones share the key handle; the handle and its certificate context are released when the
/// last clone drops.
#[derive(Clone)]
pub struct CngKey(Arc<KeyInner>);

struct KeyInner {
    key: KeyHandle,
    key_type: KeyType,
    // keeps the key (PKCS#12 keys are bound to the context) and its store alive
    _ctx: CertCtx,
    // Imported keys retain their provider; Schannel keys retain their deletion owner.
    _provider: Option<KeyHandle>,
    _owner: Option<crate::schannel::Certificate>,
}

impl CngKey {
    /// Key algorithm, taken from the leaf certificate.
    #[must_use]
    pub fn key_type(&self) -> KeyType {
        self.0.key_type
    }

    /// Hashes `msg` and signs the digest with the CNG key.
    ///
    /// # Errors
    /// [`Error::Unsupported`] if `scheme` does not match the key type, [`Error::Backend`] if
    /// CNG fails to sign (e.g. the key is not accessible or the provider rejects the padding).
    pub fn sign(&self, scheme: SignScheme, msg: &[u8]) -> Result<Vec<u8>, Error> {
        let rsa = self.0.key_type == KeyType::Rsa;
        let hash = match scheme {
            SignScheme::RsaPkcs1(h) | SignScheme::RsaPss(h) if rsa => h,
            SignScheme::Ecdsa(h) if !rsa => h,
            _ => {
                return Err(Error::Unsupported(format!(
                    "{scheme:?} for {:?} key",
                    self.0.key_type
                )));
            }
        };
        let (digest, alg) = match hash {
            HashAlg::Sha256 => (
                symcrypt::hash::sha256(msg).to_vec(),
                BCRYPT_SHA256_ALGORITHM,
            ),
            HashAlg::Sha384 => (
                symcrypt::hash::sha384(msg).to_vec(),
                BCRYPT_SHA384_ALGORITHM,
            ),
            HashAlg::Sha512 => (
                symcrypt::hash::sha512(msg).to_vec(),
                BCRYPT_SHA512_ALGORITHM,
            ),
        };
        let digest_len = u32::try_from(digest.len())
            .map_err(|_| Error::Backend("digest is too large".into()))?;
        let pkcs1 = BCRYPT_PKCS1_PADDING_INFO { pszAlgId: alg };
        let pss = BCRYPT_PSS_PADDING_INFO {
            pszAlgId: alg,
            cbSalt: digest_len,
        };
        let (padding, flags): (*const std::ffi::c_void, u32) = match scheme {
            SignScheme::RsaPkcs1(_) => ((&raw const pkcs1).cast(), NCRYPT_PAD_PKCS1_FLAG),
            SignScheme::RsaPss(_) => ((&raw const pss).cast(), NCRYPT_PAD_PSS_FLAG),
            SignScheme::Ecdsa(_) => (ptr::null(), 0),
        };
        let flags = flags | NCRYPT_SILENT_FLAG;

        let sign = |out: *mut u8, cap: u32, written: &mut u32| {
            // SAFETY: the key handle is live, `padding` points to a padding struct (or is null)
            // valid for the call, `digest` is valid for its length and `out` for `cap` bytes.
            let hr = unsafe {
                NCryptSignHash(
                    self.0.key.0,
                    padding,
                    digest.as_ptr(),
                    digest_len,
                    out,
                    cap,
                    written,
                    flags,
                )
            };
            if hr < 0 {
                Err(Error::Backend(format!("NCryptSignHash failed: 0x{hr:08x}")))
            } else {
                Ok(())
            }
        };
        let mut len = 0u32;
        sign(ptr::null_mut(), 0, &mut len)?;
        let mut sig = vec![0u8; len as usize];
        sign(sig.as_mut_ptr(), len, &mut len)?;
        sig.truncate(len as usize);

        Ok(if rsa {
            sig
        } else {
            crate::x509::identity::ecdsa_p1363_to_der(&sig)
        })
    }
}

impl fmt::Debug for CngKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CngKey")
            .field("key_type", &self.0.key_type)
            .finish_non_exhaustive()
    }
}

/// Loads the identity from a DER PKCS#12 archive: leaf-first chain without root, and its key.
pub(crate) fn from_pkcs12(der: &[u8], password: &str) -> Result<(Vec<Certificate>, CngKey), Error> {
    let blob = CRYPT_INTEGER_BLOB {
        cbData: u32::try_from(der.len())
            .map_err(|_| Error::InvalidIdentity("PKCS#12 archive is too large".into()))?,
        pbData: der.as_ptr().cast_mut(),
    };
    let flags = PKCS12_NO_PERSIST_KEY | PKCS12_ALWAYS_CNG_KSP;
    let wide_pw = wide(password);
    // SAFETY: `blob` points to `der`, the password is NUL terminated.
    let mut store = unsafe { PFXImportCertStore(&raw const blob, wide_pw.as_ptr(), flags) };
    if store.is_null() && password.is_empty() {
        // archives exported without a password may be encrypted with a NULL password
        // SAFETY: as above, a NULL password is allowed.
        store = unsafe { PFXImportCertStore(&raw const blob, ptr::null(), flags) };
    }
    if store.is_null() {
        return Err(Error::InvalidIdentity(format!(
            "PFXImportCertStore failed: {}",
            std::io::Error::last_os_error()
        )));
    }
    let store = Store(store);

    let mut cert: *mut CERT_CONTEXT = ptr::null_mut();
    loop {
        // SAFETY: `store` is open; the previous context is freed by the call.
        cert = unsafe { CertEnumCertificatesInStore(store.0, cert) };
        if cert.is_null() {
            return Err(Error::InvalidIdentity(
                "PKCS#12 archive has no certificate with a usable private key".into(),
            ));
        }
        if let Some(key) = acquire_key(cert) {
            // enumeration stops here, the context is owned by the caller
            return identity(CertCtx(cert), key, ptr::null_mut(), store.0, false, None);
        }
    }
}

/// Loads the identity from a Windows system certificate store, see [`crate::x509::identity`].
pub(crate) fn from_store(
    location: StoreLocation,
    name: &str,
    select: &StoreSelector,
) -> Result<(Vec<Certificate>, CngKey), Error> {
    let (flags, engine) = match location {
        StoreLocation::CurrentUser => (CERT_SYSTEM_STORE_CURRENT_USER, ptr::null_mut()),
        StoreLocation::LocalMachine => (CERT_SYSTEM_STORE_LOCAL_MACHINE, HCCE_LOCAL_MACHINE),
    };
    let wide_name = wide(name);
    // SAFETY: the system provider takes a NUL terminated wide store name.
    let store = unsafe {
        CertOpenStore(
            CERT_STORE_PROV_SYSTEM_W,
            0,
            0,
            flags | CERT_STORE_READONLY_FLAG | CERT_STORE_OPEN_EXISTING_FLAG,
            wide_name.as_ptr().cast(),
        )
    };
    if store.is_null() {
        return Err(Error::NotFound(format!(
            "certificate store {name}: {}",
            std::io::Error::last_os_error()
        )));
    }
    let store = Store(store);
    let encoding = X509_ASN_ENCODING | PKCS_7_ASN_ENCODING;

    let (ctx, key) = match select {
        StoreSelector::Thumbprint(tp) => {
            let hash = CRYPT_INTEGER_BLOB {
                cbData: 20,
                pbData: tp.as_ptr().cast_mut(),
            };
            // SAFETY: `hash` points to the 20-byte thumbprint for the call.
            let cert = unsafe {
                CertFindCertificateInStore(
                    store.0,
                    encoding,
                    0,
                    CERT_FIND_SHA1_HASH,
                    (&raw const hash).cast(),
                    ptr::null(),
                )
            };
            if cert.is_null() {
                return Err(Error::NotFound(format!(
                    "certificate {} is not found in {name}",
                    hex::encode(tp)
                )));
            }
            let ctx = CertCtx(cert);
            let key = acquire_key(ctx.0).ok_or_else(|| {
                Error::InvalidIdentity(format!(
                    "certificate {} has no accessible private key",
                    hex::encode(tp)
                ))
            })?;
            (ctx, key)
        }
        StoreSelector::Subject(subject) => {
            let wide_subject = wide(subject);
            let mut found: Option<(u64, CertCtx, KeyHandle)> = None;
            let mut cert: *mut CERT_CONTEXT = ptr::null_mut();
            loop {
                // SAFETY: `store` is open, the subject is NUL terminated; the previous context is
                // freed by the call.
                cert = unsafe {
                    CertFindCertificateInStore(
                        store.0,
                        encoding,
                        0,
                        CERT_FIND_SUBJECT_STR_W,
                        wide_subject.as_ptr().cast(),
                        cert,
                    )
                };
                if cert.is_null() {
                    break;
                }
                // SAFETY: `cert` is a live context with a valid `pCertInfo`.
                let info = unsafe { (*cert).pCertInfo };
                // SAFETY: null time means the current time, `info` is valid.
                if unsafe { CertVerifyTimeValidity(ptr::null(), info) } != 0 {
                    continue;
                }
                // SAFETY: as above.
                let not_after = unsafe { (*info).NotAfter };
                let not_after = (u64::from(not_after.dwHighDateTime) << 32)
                    | u64::from(not_after.dwLowDateTime);
                if found.as_ref().is_some_and(|(last, ..)| not_after <= *last) {
                    continue;
                }
                if let Some(key) = acquire_key(cert) {
                    // SAFETY: `cert` is live; the duplicate is owned, the find context is not.
                    found = Some((
                        not_after,
                        CertCtx(unsafe { CertDuplicateCertificateContext(cert) }),
                        key,
                    ));
                }
            }
            let (_, ctx, key) = found.ok_or_else(|| {
                Error::NotFound(format!(
                    "no valid certificate with an accessible private key matches subject {subject:?} in {name}"
                ))
            })?;
            (ctx, key)
        }
    };
    identity(ctx, key, engine, ptr::null_mut(), false, None)
}

/// Acquires a native key while retaining the Schannel owner of any temporary persisted key.
pub(crate) fn from_schannel(
    cert: crate::schannel::Certificate,
) -> Result<(Vec<Certificate>, CngKey), Error> {
    let key = acquire_key(cert.as_ptr())
        .ok_or_else(|| Error::InvalidIdentity("certificate has no accessible CNG key".into()))?;
    // SAFETY: the Schannel owner keeps the context alive; the duplicate is separately owned.
    let ctx = CertCtx(unsafe { CertDuplicateCertificateContext(cert.as_ptr()) });
    identity(ctx, key, ptr::null_mut(), ptr::null_mut(), true, Some(cert))
}

/// Imports a PKCS#8 key into an ephemeral CNG handle and checks its public key.
pub(crate) fn from_pkcs8(cert: &Certificate, der: &[u8]) -> Result<CngKey, Error> {
    use windows_sys::Win32::Security::Cryptography::{
        MS_KEY_STORAGE_PROVIDER, NCRYPT_PKCS8_PRIVATE_KEY_BLOB, NCryptImportKey,
        NCryptOpenStorageProvider,
    };

    let key_type = cert
        .key_type()
        .ok_or_else(|| Error::Unsupported("native key algorithm".into()))?;
    let mut provider = 0;
    // SAFETY: the provider name and output pointer are valid.
    status(unsafe { NCryptOpenStorageProvider(&raw mut provider, MS_KEY_STORAGE_PROVIDER, 0) })?;
    let provider = KeyHandle(provider, true);
    let mut key = 0;
    let len = u32::try_from(der.len())
        .map_err(|_| Error::InvalidIdentity("private key is too large".into()))?;
    // SAFETY: buffers are valid; no name or export policy is supplied, so the key is ephemeral
    // and its private material is not exportable.
    status(unsafe {
        NCryptImportKey(
            provider.0,
            0,
            NCRYPT_PKCS8_PRIVATE_KEY_BLOB,
            ptr::null(),
            &raw mut key,
            der.as_ptr(),
            len,
            NCRYPT_SILENT_FLAG,
        )
    })?;
    let key = KeyHandle(key, true);
    if public_key_der(key.0)? != cert.public_key_der() {
        return Err(Error::InvalidIdentity(
            "certificate/private key mismatch".into(),
        ));
    }
    Ok(CngKey(Arc::new(KeyInner {
        key,
        key_type,
        _ctx: CertCtx::from_der(cert.der())?,
        _provider: Some(provider),
        _owner: None,
    })))
}

/// Exports only public `SubjectPublicKeyInfo`, never private key material.
fn public_key_der(key: NCRYPT_KEY_HANDLE) -> Result<Vec<u8>, Error> {
    use windows_sys::Win32::Security::Cryptography::{
        CryptEncodeObjectEx, CryptExportPublicKeyInfo, X509_PUBLIC_KEY_INFO,
    };
    let mut len = 0;
    // SAFETY: the key is live; null output queries the required allocation size.
    win32(unsafe {
        CryptExportPublicKeyInfo(
            key,
            CERT_NCRYPT_KEY_SPEC,
            X509_ASN_ENCODING,
            ptr::null_mut(),
            &raw mut len,
        )
    })?;
    let mut info = vec![0usize; (len as usize).div_ceil(mem::size_of::<usize>())];
    // SAFETY: the allocation has the requested size and pointer alignment.
    win32(unsafe {
        CryptExportPublicKeyInfo(
            key,
            CERT_NCRYPT_KEY_SPEC,
            X509_ASN_ENCODING,
            info.as_mut_ptr().cast(),
            &raw mut len,
        )
    })?;
    // SAFETY: info contains a live CERT_PUBLIC_KEY_INFO and output size is queried first.
    win32(unsafe {
        CryptEncodeObjectEx(
            X509_ASN_ENCODING,
            X509_PUBLIC_KEY_INFO,
            info.as_ptr().cast(),
            0,
            ptr::null(),
            ptr::null_mut(),
            &raw mut len,
        )
    })?;
    let mut der = vec![0; len as usize];
    // SAFETY: output allocation is valid for len bytes; info remains alive.
    win32(unsafe {
        CryptEncodeObjectEx(
            X509_ASN_ENCODING,
            X509_PUBLIC_KEY_INFO,
            info.as_ptr().cast(),
            0,
            ptr::null(),
            der.as_mut_ptr().cast(),
            &raw mut len,
        )
    })?;
    der.truncate(len as usize);
    Ok(der)
}

fn status(code: i32) -> Result<(), Error> {
    if code == 0 {
        Ok(())
    } else {
        Err(Error::Backend(format!("CNG failed: 0x{code:08x}")))
    }
}

fn win32(ok: i32) -> Result<(), Error> {
    if ok != 0 {
        Ok(())
    } else {
        Err(Error::Backend(std::io::Error::last_os_error().to_string()))
    }
}

/// Verifies a certificate signature with the supplied issuer, without trust evaluation.
pub(crate) fn verify_signature(cert: &Certificate, issuer: &Certificate) -> Result<bool, Error> {
    let cert = CertCtx::from_der(cert.der())?;
    let issuer = CertCtx::from_der(issuer.der())?;
    // SAFETY: both owned contexts outlive the call and match the specified subject types.
    Ok(unsafe {
        CryptVerifyCertificateSignatureEx(
            0,
            X509_ASN_ENCODING | PKCS_7_ASN_ENCODING,
            CRYPT_VERIFY_CERT_SIGN_SUBJECT_CERT,
            cert.0.cast(),
            CRYPT_VERIFY_CERT_SIGN_ISSUER_CERT,
            issuer.0.cast(),
            0,
            ptr::null_mut(),
        )
    } != 0)
}

fn identity(
    ctx: CertCtx,
    key: KeyHandle,
    engine: HCERTCHAINENGINE,
    extra: HCERTSTORE,
    include_root: bool,
    owner: Option<crate::schannel::Certificate>,
) -> Result<(Vec<Certificate>, CngKey), Error> {
    let chain = build_chain(&ctx, engine, extra, include_root)?;
    if public_key_der(key.0)? != chain[0].public_key_der() {
        return Err(Error::InvalidIdentity(
            "certificate/private key mismatch".into(),
        ));
    }
    let key_type = chain[0].key_type().ok_or_else(|| {
        Error::Unsupported(format!(
            "certificate key algorithm of {}",
            chain[0].subject()
        ))
    })?;
    Ok((
        chain,
        CngKey(Arc::new(KeyInner {
            key,
            key_type,
            _ctx: ctx,
            _provider: None,
            _owner: owner,
        })),
    ))
}

/// Builds the presented chain, leaf first, without a self-signed root.
fn build_chain(
    leaf: &CertCtx,
    engine: HCERTCHAINENGINE,
    extra: HCERTSTORE,
    include_root: bool,
) -> Result<Vec<Certificate>, Error> {
    // SAFETY: all-zero is a valid `CERT_CHAIN_PARA` (no usage requested).
    let mut para: CERT_CHAIN_PARA = unsafe { mem::zeroed() };
    para.cbSize = u32::try_from(mem::size_of::<CERT_CHAIN_PARA>()).expect("fits u32");
    let mut chain: *mut CERT_CHAIN_CONTEXT = ptr::null_mut();
    // SAFETY: all pointers are valid for the call; `chain` receives an owned context.
    let ok = unsafe {
        CertGetCertificateChain(
            engine,
            leaf.0,
            ptr::null(),
            extra,
            &raw const para,
            CERT_CHAIN_DISABLE_AIA | CERT_CHAIN_CACHE_ONLY_URL_RETRIEVAL,
            ptr::null(),
            &raw mut chain,
        )
    };
    if ok == 0 {
        return Err(Error::Backend(format!(
            "CertGetCertificateChain failed: {}",
            std::io::Error::last_os_error()
        )));
    }
    let chain = ChainCtx(chain);

    // SAFETY: a successful call returns a valid chain context.
    let ctx = unsafe { &*chain.0 };
    let mut certs = Vec::new();
    if ctx.cChain > 0 {
        // SAFETY: `rgpChain` has `cChain` (>= 1) valid entries, each with `cElement` elements.
        let simple = unsafe { &**ctx.rgpChain };
        let elements =
            unsafe { std::slice::from_raw_parts(simple.rgpElement, simple.cElement as usize) };
        for &el in elements {
            // SAFETY: elements and their certificate contexts are valid while `chain` lives.
            certs.push(Certificate::from_der(ntex_bytes::Bytes::copy_from_slice(
                unsafe { ctx_der((*el).pCertContext) },
            ))?);
        }
        if !include_root && certs.len() > 1 && certs.last().is_some_and(Certificate::is_self_issued)
        {
            // SAFETY: the last certificate context is live while `chain` is owned here.
            let root = unsafe { (**elements.last().unwrap()).pCertContext };
            // SAFETY: both pointers are live certificate contexts of the indicated types.
            let self_signed = unsafe {
                CryptVerifyCertificateSignatureEx(
                    0,
                    X509_ASN_ENCODING | PKCS_7_ASN_ENCODING,
                    CRYPT_VERIFY_CERT_SIGN_SUBJECT_CERT,
                    root.cast(),
                    CRYPT_VERIFY_CERT_SIGN_ISSUER_CERT,
                    root.cast(),
                    0,
                    ptr::null_mut(),
                )
            };
            if self_signed != 0 {
                certs.pop();
            }
        }
    }
    // SAFETY: `leaf` is a live context.
    let leaf_der = unsafe { ctx_der(leaf.0) };
    if certs.first().is_none_or(|c| c.der().as_ref() != leaf_der) {
        return Err(Error::Backend(
            "certificate chain does not start with the leaf".into(),
        ));
    }
    Ok(certs)
}

/// DER encoding of a live certificate context.
///
/// # Safety
/// `ctx` must be a valid context that outlives the returned slice.
unsafe fn ctx_der<'a>(ctx: *const CERT_CONTEXT) -> &'a [u8] {
    // SAFETY: guaranteed by the caller.
    unsafe { std::slice::from_raw_parts((*ctx).pbCertEncoded, (*ctx).cbCertEncoded as usize) }
}

/// Acquires the CNG private key of a live certificate context without UI.
fn acquire_key(cert: *const CERT_CONTEXT) -> Option<KeyHandle> {
    let mut handle = 0;
    let mut spec: CERT_KEY_SPEC = 0;
    let mut free = 0;
    // SAFETY: `cert` is live, the out pointers are valid.
    let ok = unsafe {
        windows_sys::Win32::Security::Cryptography::CryptAcquireCertificatePrivateKey(
            cert,
            CRYPT_ACQUIRE_ONLY_NCRYPT_KEY_FLAG | CRYPT_ACQUIRE_SILENT_FLAG,
            ptr::null(),
            &raw mut handle,
            &raw mut spec,
            &raw mut free,
        )
    };
    if ok == 0 {
        return None;
    }
    let key = KeyHandle(handle as NCRYPT_KEY_HANDLE, free != 0);
    (spec == CERT_NCRYPT_KEY_SPEC).then_some(key)
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// CNG key handle, freed on drop if the caller owns it (`pfCallerFreeProvOrNCryptKey`).
struct KeyHandle(NCRYPT_KEY_HANDLE, bool);

impl Drop for KeyHandle {
    fn drop(&mut self) {
        if self.1 {
            // SAFETY: the handle is owned.
            unsafe { NCryptFreeObject(self.0) };
        }
    }
}

/// Owned certificate store handle; open certificate contexts keep the store alive.
struct Store(HCERTSTORE);

impl Drop for Store {
    fn drop(&mut self) {
        // SAFETY: handle is owned and open.
        unsafe { CertCloseStore(self.0, 0) };
    }
}

/// Owned certificate context.
struct CertCtx(*mut CERT_CONTEXT);

impl CertCtx {
    fn from_der(der: &[u8]) -> Result<Self, Error> {
        let len = u32::try_from(der.len())
            .map_err(|_| Error::InvalidDer("certificate is too large".into()))?;
        // SAFETY: DER is readable for len bytes; the API copies it into an owned context.
        let ctx = unsafe {
            windows_sys::Win32::Security::Cryptography::CertCreateCertificateContext(
                X509_ASN_ENCODING,
                der.as_ptr(),
                len,
            )
        };
        if ctx.is_null() {
            Err(Error::InvalidDer(
                std::io::Error::last_os_error().to_string(),
            ))
        } else {
            Ok(Self(ctx))
        }
    }
}

impl Drop for CertCtx {
    fn drop(&mut self) {
        // SAFETY: context is owned and non-null.
        unsafe { CertFreeCertificateContext(self.0) };
    }
}

/// Owned chain context.
struct ChainCtx(*mut CERT_CHAIN_CONTEXT);

impl Drop for ChainCtx {
    fn drop(&mut self) {
        // SAFETY: context is owned and non-null.
        unsafe { CertFreeCertificateChain(self.0) };
    }
}

// SAFETY: CNG key handles and certificate contexts are thread safe (CNG/Crypt32 synchronize
// internally); the key is only used through `NCryptSignHash` and both are released on drop.
unsafe impl Send for KeyInner {}
// SAFETY: see `Send`.
unsafe impl Sync for KeyInner {}
