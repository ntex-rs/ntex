//! CNG client identity (Windows).
//!
//! The private key is never exported: it stays in its CNG key storage provider (software KSP,
//! TPM, ...) and every TLS handshake signature is made in place with `NCryptSignHash`. The
//! message is hashed with CNG (`BCrypt`), so no OpenSSL is involved on Windows.
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
    CERT_CHAIN_DISABLE_AIA, CERT_CHAIN_PARA, CERT_KEY_SPEC, CERT_NCRYPT_KEY_SPEC,
    CERT_SYSTEM_STORE_CURRENT_USER, CERT_SYSTEM_STORE_LOCAL_MACHINE,
    CRYPT_ACQUIRE_ONLY_NCRYPT_KEY_FLAG, CRYPT_ACQUIRE_SILENT_FLAG, HCERTCHAINENGINE,
    NCRYPT_KEY_HANDLE, NCRYPT_PAD_PKCS1_FLAG, NCRYPT_PAD_PSS_FLAG, NCRYPT_SILENT_FLAG,
    NCryptSignHash, PKCS12_ALWAYS_CNG_KSP, PKCS12_NO_PERSIST_KEY, X509_ASN_ENCODING,
};

use crate::win::{Cert, CertContext, CertStore, ChainContext, NcryptHandle};
use crate::x509::digest::cng as digest;
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

// Fields drop in order: the key before the context or provider it may belong to.
struct KeyInner {
    key: KeyHandle,
    key_type: KeyType,
    // keeps the key (PKCS#12 keys are bound to the context) and its store alive
    _ctx: CertContext,
    // Imported keys retain their provider; Schannel keys retain their deletion owner.
    _provider: Option<NcryptHandle>,
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
                digest::hash::<32>(digest::BCRYPT_SHA256_ALG_HANDLE, msg).to_vec(),
                BCRYPT_SHA256_ALGORITHM,
            ),
            HashAlg::Sha384 => (
                digest::hash::<48>(digest::BCRYPT_SHA384_ALG_HANDLE, msg).to_vec(),
                BCRYPT_SHA384_ALGORITHM,
            ),
            HashAlg::Sha512 => (
                digest::hash::<64>(digest::BCRYPT_SHA512_ALG_HANDLE, msg).to_vec(),
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
                    self.0.key.raw(),
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

/// Checks that `der` is a PKCS#12 archive `PFXImportCertStore` accepts.
///
/// # Errors
/// [`Error::InvalidDer`] if `der` is not a PKCS#12 archive (same as the Unix decoder).
pub(crate) fn check_pfx(der: &[u8]) -> Result<(), Error> {
    if u32::try_from(der.len()).is_err() {
        return Err(Error::InvalidIdentity(
            "PKCS#12 archive is too large".into(),
        ));
    }
    if !crate::win::is_pfx(der) {
        return Err(Error::InvalidDer("not a PKCS#12 archive".into()));
    }
    Ok(())
}

/// Loads the identity from a DER PKCS#12 archive: leaf-first chain without root, and its key.
pub(crate) fn from_pkcs12(der: &[u8], password: &str) -> Result<(Vec<Certificate>, CngKey), Error> {
    check_pfx(der)?;
    let flags = PKCS12_NO_PERSIST_KEY | PKCS12_ALWAYS_CNG_KSP;
    let mut store = CertStore::from_pfx(der, Some(password), flags);
    if store.is_err() && password.is_empty() {
        // archives exported without a password may be encrypted with a NULL password
        store = CertStore::from_pfx(der, None, flags);
    }
    let store =
        store.map_err(|e| Error::InvalidIdentity(format!("PFXImportCertStore failed: {e}")))?;
    let (ctx, key) = store.find_map(acquire_key).ok_or_else(|| {
        Error::InvalidIdentity(
            "PKCS#12 archive has no certificate with a usable private key".into(),
        )
    })?;
    identity(ctx, key, ptr::null_mut(), Some(&store), false, None)
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
    let store = CertStore::open_system(flags, name)
        .map_err(|e| Error::NotFound(format!("certificate store {name}: {e}")))?;

    let (ctx, key) = match select {
        StoreSelector::Thumbprint(tp) => {
            let ctx = store.find_sha1(tp).ok_or_else(|| {
                Error::NotFound(format!(
                    "certificate {} is not found in {name}",
                    hex::encode(tp)
                ))
            })?;
            let key = acquire_key(&ctx).ok_or_else(|| {
                Error::InvalidIdentity(format!(
                    "certificate {} has no accessible private key",
                    hex::encode(tp)
                ))
            })?;
            (ctx, key)
        }
        StoreSelector::Subject(subject) => {
            let mut found: Option<(u64, CertContext, KeyHandle)> = None;
            store.for_each_subject(subject, |cert| {
                if !cert.is_time_valid() {
                    return;
                }
                let not_after = cert.not_after();
                if found.as_ref().is_some_and(|(last, ..)| not_after <= *last) {
                    return;
                }
                if let Some(key) = acquire_key(cert) {
                    found = Some((not_after, cert.duplicate(), key));
                }
            });
            let (_, ctx, key) = found.ok_or_else(|| {
                Error::NotFound(format!(
                    "no valid certificate with an accessible private key matches subject {subject:?} in {name}"
                ))
            })?;
            (ctx, key)
        }
    };
    identity(ctx, key, engine, None, false, None)
}

/// Acquires a native key while retaining the Schannel owner of any temporary persisted key.
pub(crate) fn from_schannel(
    cert: crate::schannel::Certificate,
) -> Result<(Vec<Certificate>, CngKey), Error> {
    let key = acquire_key(cert.context())
        .ok_or_else(|| Error::InvalidIdentity("certificate has no accessible CNG key".into()))?;
    let ctx = cert.context().duplicate();
    identity(ctx, key, ptr::null_mut(), None, true, Some(cert))
}

/// Imports a PKCS#8 key into an ephemeral CNG handle and checks its public key.
pub(crate) fn from_pkcs8(cert: &Certificate, der: &[u8]) -> Result<CngKey, Error> {
    use windows_sys::Win32::Security::Cryptography::{
        NCRYPT_PKCS8_PRIVATE_KEY_BLOB, NCryptImportKey,
    };

    let key_type = cert
        .key_type()
        .ok_or_else(|| Error::Unsupported("native key algorithm".into()))?;
    let provider = NcryptHandle::software_provider().map_err(cng_error)?;
    let mut key = 0;
    let len = u32::try_from(der.len())
        .map_err(|_| Error::InvalidIdentity("private key is too large".into()))?;
    // SAFETY: buffers are valid; no name or export policy is supplied, so the key is ephemeral
    // and its private material is not exportable.
    status(unsafe {
        NCryptImportKey(
            provider.as_raw(),
            0,
            NCRYPT_PKCS8_PRIVATE_KEY_BLOB,
            ptr::null(),
            &raw mut key,
            der.as_ptr(),
            len,
            NCRYPT_SILENT_FLAG,
        )
    })?;
    // SAFETY: a successful import returns a handle owned by the caller.
    let key = KeyHandle::Owned(unsafe { NcryptHandle::from_raw(key) });
    if public_key_der(key.raw())? != cert.public_key_der() {
        return Err(Error::InvalidIdentity(
            "certificate/private key mismatch".into(),
        ));
    }
    Ok(CngKey(Arc::new(KeyInner {
        key,
        key_type,
        _ctx: cert_context(cert)?,
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

pub(crate) fn status(code: i32) -> Result<(), Error> {
    if code == 0 { Ok(()) } else { Err(cng_error(code)) }
}

fn cng_error(code: i32) -> Error {
    Error::Backend(format!("CNG failed: 0x{code:08x}"))
}

pub(crate) fn win32(ok: i32) -> Result<(), Error> {
    if ok != 0 {
        Ok(())
    } else {
        Err(Error::Backend(std::io::Error::last_os_error().to_string()))
    }
}

/// Verifies a certificate signature with the supplied issuer, without trust evaluation.
pub(crate) fn verify_signature(cert: &Certificate, issuer: &Certificate) -> Result<bool, Error> {
    let issuer = cert_context(issuer)?;
    Ok(cert_context(cert)?.is_signed_by(&issuer))
}

fn cert_context(cert: &Certificate) -> Result<CertContext, Error> {
    CertContext::from_der(cert.der()).map_err(|e| Error::InvalidDer(e.to_string()))
}

fn identity(
    ctx: CertContext,
    key: KeyHandle,
    engine: HCERTCHAINENGINE,
    extra: Option<&CertStore>,
    include_root: bool,
    owner: Option<crate::schannel::Certificate>,
) -> Result<(Vec<Certificate>, CngKey), Error> {
    let chain = build_chain(&ctx, engine, extra, include_root)?;
    if public_key_der(key.raw())? != chain[0].public_key_der() {
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
    leaf: &Cert,
    engine: HCERTCHAINENGINE,
    extra: Option<&CertStore>,
    include_root: bool,
) -> Result<Vec<Certificate>, Error> {
    // SAFETY: all-zero is a valid `CERT_CHAIN_PARA` (no usage requested).
    let mut para: CERT_CHAIN_PARA = unsafe { mem::zeroed() };
    para.cbSize = u32::try_from(mem::size_of::<CERT_CHAIN_PARA>()).expect("fits u32");
    // SAFETY: `engine` is the default (null) or the predefined local machine engine, `para`
    // holds no pointers.
    let chain = unsafe {
        ChainContext::build(
            engine,
            leaf,
            None,
            extra,
            &para,
            CERT_CHAIN_DISABLE_AIA | CERT_CHAIN_CACHE_ONLY_URL_RETRIEVAL,
        )
    }
    .map_err(|e| Error::Backend(format!("CertGetCertificateChain failed: {e}")))?;

    let elements = chain.certificates();
    let mut certs = elements
        .iter()
        .map(|cert| Certificate::from_der(ntex_bytes::Bytes::copy_from_slice(cert.der())))
        .collect::<Result<Vec<_>, _>>()?;
    if !include_root
        && certs.len() > 1
        && certs.last().is_some_and(Certificate::is_self_issued)
        && elements.last().is_some_and(|root| root.is_signed_by(root))
    {
        certs.pop();
    }
    if certs.first().is_none_or(|c| c.der().as_ref() != leaf.der()) {
        return Err(Error::Backend(
            "certificate chain does not start with the leaf".into(),
        ));
    }
    Ok(certs)
}

/// Acquires the CNG private key of a certificate context without UI.
fn acquire_key(cert: &Cert) -> Option<KeyHandle> {
    let mut handle = 0;
    let mut spec: CERT_KEY_SPEC = 0;
    let mut free = 0;
    // SAFETY: `cert` is live, the out pointers are valid.
    let ok = unsafe {
        windows_sys::Win32::Security::Cryptography::CryptAcquireCertificatePrivateKey(
            cert.as_ptr(),
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
    let key = if free == 0 {
        KeyHandle::Borrowed(handle)
    } else {
        // SAFETY: `pfCallerFreeProvOrNCryptKey` set, the caller owns the handle.
        KeyHandle::Owned(unsafe { NcryptHandle::from_raw(handle) })
    };
    (spec == CERT_NCRYPT_KEY_SPEC).then_some(key)
}

/// CNG key handle, owned or cached by its certificate context (`pfCallerFreeProvOrNCryptKey`).
enum KeyHandle {
    Owned(NcryptHandle),
    Borrowed(NCRYPT_KEY_HANDLE),
}

impl KeyHandle {
    fn raw(&self) -> NCRYPT_KEY_HANDLE {
        match self {
            Self::Owned(handle) => handle.as_raw(),
            Self::Borrowed(handle) => *handle,
        }
    }
}
