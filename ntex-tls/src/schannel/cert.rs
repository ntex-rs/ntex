use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::{io, ptr, slice};

use windows_sys::Win32::Foundation::SEC_E_OK;
use windows_sys::Win32::Security::Cryptography::{
    BCRYPTBUFFER_VERSION, BCryptBuffer, BCryptBufferDesc, CERT_CONTEXT, CERT_KEY_PROV_INFO_PROP_ID,
    CRYPT_KEY_PROV_INFO, CRYPT_STRING_BASE64HEADER, CertCreateCertificateContext,
    CertDuplicateCertificateContext, CertFreeCertificateContext, CertSetCertificateContextProperty,
    CryptStringToBinaryA, MS_KEY_STORAGE_PROVIDER, NCRYPT_KEY_HANDLE, NCRYPT_OVERWRITE_KEY_FLAG,
    NCRYPT_PKCS8_PRIVATE_KEY_BLOB, NCRYPT_PROV_HANDLE, NCRYPT_SILENT_FLAG,
    NCRYPTBUFFER_PKCS_KEY_NAME, NCryptDeleteKey, NCryptFreeObject, NCryptImportKey,
    NCryptOpenStorageProvider, PKCS_7_ASN_ENCODING, X509_ASN_ENCODING,
};

static KEY_ID: AtomicU64 = AtomicU64::new(1);

struct PersistedKey(NCRYPT_KEY_HANDLE);

impl Drop for PersistedKey {
    fn drop(&mut self) {
        if self.0 != 0 {
            unsafe {
                NCryptDeleteKey(self.0, 0);
            }
        }
    }
}

/// Windows Schannel server configuration.
///
/// Holds a certificate with an associated private key used to accept TLS
/// connections.
pub struct ServerConfig {
    cert: *mut CERT_CONTEXT,
    key: Arc<PersistedKey>,
}

unsafe impl Send for ServerConfig {}
unsafe impl Sync for ServerConfig {}

impl Clone for ServerConfig {
    fn clone(&self) -> Self {
        Self {
            cert: unsafe { CertDuplicateCertificateContext(self.cert) },
            key: self.key.clone(),
        }
    }
}

impl Drop for ServerConfig {
    fn drop(&mut self) {
        if !self.cert.is_null() {
            unsafe {
                CertFreeCertificateContext(self.cert);
            }
        }
    }
}

impl std::fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerConfig").finish()
    }
}

impl ServerConfig {
    /// Load a PEM-encoded certificate and PKCS#8 private key.
    pub fn from_pem(cert_pem: &str, key_pem: &str) -> io::Result<Self> {
        let cert_der = decode_pem(cert_pem)?;
        let key_der = decode_pem(key_pem)?;

        let cert = unsafe {
            CertCreateCertificateContext(
                X509_ASN_ENCODING | PKCS_7_ASN_ENCODING,
                cert_der.as_ptr(),
                u32::try_from(cert_der.len())
                    .map_err(|_| io::Error::other("TLS certificate is too large"))?,
            )
        };
        if cert.is_null() {
            return Err(io::Error::last_os_error());
        }

        match attach_private_key(cert, &key_der) {
            Ok(key) => Ok(Self {
                cert,
                key: Arc::new(key),
            }),
            Err(err) => {
                unsafe {
                    CertFreeCertificateContext(cert);
                }
                Err(err)
            }
        }
    }

    /// Certificate in DER encoding.
    #[must_use]
    pub fn cert_der(&self) -> Vec<u8> {
        unsafe {
            let cert = &*self.cert;
            slice::from_raw_parts(cert.pbCertEncoded, cert.cbCertEncoded as usize).to_vec()
        }
    }

    pub(super) fn cert(&self) -> *mut CERT_CONTEXT {
        self.cert
    }
}

fn decode_pem(pem: &str) -> io::Result<Vec<u8>> {
    let bytes = pem.as_bytes();
    let mut len = 0u32;
    let ok = unsafe {
        CryptStringToBinaryA(
            bytes.as_ptr(),
            u32::try_from(bytes.len()).map_err(|_| io::Error::other("PEM is too large"))?,
            CRYPT_STRING_BASE64HEADER,
            ptr::null_mut(),
            &raw mut len,
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }

    let mut buf = vec![0u8; len as usize];
    let ok = unsafe {
        CryptStringToBinaryA(
            bytes.as_ptr(),
            u32::try_from(bytes.len()).map_err(|_| io::Error::other("PEM is too large"))?,
            CRYPT_STRING_BASE64HEADER,
            buf.as_mut_ptr(),
            &raw mut len,
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    buf.truncate(len as usize);
    Ok(buf)
}

fn attach_private_key(cert: *mut CERT_CONTEXT, key_der: &[u8]) -> io::Result<PersistedKey> {
    let id = KEY_ID.fetch_add(1, Ordering::Relaxed);
    let mut name: Vec<u16> = format!("ntex-schannel-{}-{}", std::process::id(), id)
        .encode_utf16()
        .chain(Some(0))
        .collect();

    let mut provider: NCRYPT_PROV_HANDLE = 0;
    let status =
        unsafe { NCryptOpenStorageProvider(&raw mut provider, MS_KEY_STORAGE_PROVIDER, 0) };
    if status != SEC_E_OK {
        return Err(io::Error::from_raw_os_error(status));
    }

    let mut name_buf = BCryptBuffer {
        cbBuffer: u32::try_from(name.len() * 2).expect("key name fits u32"),
        BufferType: NCRYPTBUFFER_PKCS_KEY_NAME,
        pvBuffer: name.as_mut_ptr().cast(),
    };
    let params = BCryptBufferDesc {
        ulVersion: BCRYPTBUFFER_VERSION,
        cBuffers: 1,
        pBuffers: &raw mut name_buf,
    };

    let mut key: NCRYPT_KEY_HANDLE = 0;
    let status = unsafe {
        NCryptImportKey(
            provider,
            0,
            NCRYPT_PKCS8_PRIVATE_KEY_BLOB,
            &raw const params,
            &raw mut key,
            key_der.as_ptr(),
            u32::try_from(key_der.len()).map_err(|_| io::Error::other("TLS key is too large"))?,
            NCRYPT_OVERWRITE_KEY_FLAG | NCRYPT_SILENT_FLAG,
        )
    };
    unsafe {
        NCryptFreeObject(provider);
    }
    if status != SEC_E_OK {
        return Err(io::Error::from_raw_os_error(status));
    }

    let mut prov_name: Vec<u16> = {
        let mut s = Vec::new();
        let mut p = MS_KEY_STORAGE_PROVIDER;
        unsafe {
            while *p != 0 {
                s.push(*p);
                p = p.add(1);
            }
        }
        s.push(0);
        s
    };
    let prov_info = CRYPT_KEY_PROV_INFO {
        pwszContainerName: name.as_mut_ptr(),
        pwszProvName: prov_name.as_mut_ptr(),
        dwProvType: 0,
        dwFlags: 0,
        cProvParam: 0,
        rgProvParam: ptr::null_mut(),
        dwKeySpec: 0,
    };
    let ok = unsafe {
        CertSetCertificateContextProperty(
            cert,
            CERT_KEY_PROV_INFO_PROP_ID,
            0,
            (&raw const prov_info).cast(),
        )
    };
    if ok == 0 {
        unsafe {
            NCryptDeleteKey(key, 0);
        }
        return Err(io::Error::last_os_error());
    }

    Ok(PersistedKey(key))
}
