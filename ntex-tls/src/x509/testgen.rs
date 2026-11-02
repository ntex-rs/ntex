//! Test certificate generators for every x509 backend.
//!
//! Enabled by the `testgen` feature (always available to this crate's unit tests). Tests need
//! certificate chains with specific names, keys, extensions and validity windows, so they are
//! generated at run time instead of being checked in. Each platform uses the library it
//! already links, so the same tests run on every supported backend:
//!
//! - Windows: native CNG keys and Crypt32 certificate encoding. No extra dependency, and
//!   generated archives are exactly what the platform itself produces.
//! - Unix with `openssl`: the OpenSSL builder, which also provides PKCS#12 archives.
//! - Other (rustls-only) builds: `rcgen` with `ring`, enabled by the separate
//!   `testgen-rcgen` feature so native builds never pull it in. It is only used where no
//!   native backend exists. [`rsa_key`] returns the fixed key from `examples/key.pem`, because `ring` cannot
//!   generate RSA keys; tests only need RSA as a different key type, not as a fresh key.
//!
//! These generators are for tests only: keys are ephemeral and exportable, and the serial
//! number is fixed. On rustls, [`client`] needs an installed crypto provider; this module
//! never installs one.
//!
//! All backends produce v3 certificates with serial `0x0a0b`, a critical CA basic constraint
//! for CAs, SANs in `dns`, `uri`, `email`, `ip` order, an optional non-critical extended key
//! usage and SHA-256 signatures. The issuer name
//! is taken from the issuer certificate: its raw subject bytes on OpenSSL and Windows, its CN
//! on `rcgen` (encoded the same way as subjects). `rcgen` adds a subject key identifier but no
//! authority key identifier, so issuer selection is still by name and signature.
//!
//! PKCS#12 archives are only available where [`Identity::from_pkcs12`] is supported;
//! [`client`] loads identities through the portable DER path instead.
//!
//! [`Identity::from_pkcs12`]: crate::x509::Identity::from_pkcs12
#![allow(clippy::missing_panics_doc)]

use std::net::IpAddr;

use crate::x509::{Certificate, ClientIdentity};

pub use self::imp::Key;

impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Key(<redacted>)")
    }
}

/// Subject alternative names of a generated certificate.
#[derive(Debug, Default)]
pub struct San<'a> {
    /// DNS names.
    pub dns: &'a [&'a str],
    /// URIs.
    pub uri: &'a [&'a str],
    /// RFC 822 email addresses.
    pub email: &'a [&'a str],
    /// IP addresses.
    pub ip: &'a [IpAddr],
}

/// Generates an EC P-256 key.
#[must_use]
pub fn key() -> Key {
    #[cfg(test)]
    crate::x509::test_provider();
    Key::ec()
}

/// Returns an RSA 2048 key (fixed on `rcgen`, see the module docs).
#[must_use]
pub fn rsa_key() -> Key {
    #[cfg(test)]
    crate::x509::test_provider();
    Key::rsa()
}

/// Extended key usage purpose of a generated certificate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtKeyUsage {
    /// TLS server authentication (`id-kp-serverAuth`).
    ServerAuth,
    /// TLS client authentication (`id-kp-clientAuth`).
    ClientAuth,
}

/// Generates a certificate for `key`, valid from/to `days` relative to now.
///
/// `issuer` None means self-signed.
#[must_use]
pub fn cert(
    cn: &str,
    ca: bool,
    issuer: Option<(&Certificate, &Key)>,
    key: &Key,
    san: &San<'_>,
    days: (i64, i64),
) -> Certificate {
    cert_with_eku(cn, ca, issuer, key, san, days, &[])
}

/// Like [`cert`], with a non-critical extended key usage extension listing `eku`.
///
/// An empty `eku` omits the extension.
#[must_use]
pub fn cert_with_eku(
    cn: &str,
    ca: bool,
    issuer: Option<(&Certificate, &Key)>,
    key: &Key,
    san: &San<'_>,
    days: (i64, i64),
    eku: &[ExtKeyUsage],
) -> Certificate {
    let now = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap();
    let validity = (now + days.0 * 86400, now + days.1 * 86400);
    Certificate::from_der(imp::cert(cn, ca, issuer, key, san, validity, eku)).unwrap()
}

/// Loads a client identity from `leaf`, its key and additional certificates in the given order.
#[must_use]
pub fn client(leaf: &Certificate, key: &Key, extras: &[&Certificate]) -> ClientIdentity {
    let mut certs = vec![leaf.der().as_ref()];
    certs.extend(extras.iter().map(|c| c.der().as_ref()));
    ClientIdentity::from_der(&certs, &key.pkcs8_der()).unwrap()
}

/// Builds a password-protected PKCS#12 archive.
#[cfg(any(windows, all(unix, feature = "openssl")))]
#[must_use]
pub fn pkcs12(leaf: &Certificate, key: &Key, extras: &[&Certificate], pw: &str) -> Vec<u8> {
    imp::pkcs12(leaf, key, extras, pw)
}

#[cfg(all(unix, feature = "openssl"))]
mod imp {
    use tls_openssl::asn1::Asn1Time;
    use tls_openssl::bn::BigNum;
    use tls_openssl::ec::{EcGroup, EcKey};
    use tls_openssl::hash::MessageDigest;
    use tls_openssl::nid::Nid;
    use tls_openssl::pkcs12::Pkcs12;
    use tls_openssl::pkey::{PKey, Private};
    use tls_openssl::rsa::Rsa;
    use tls_openssl::stack::Stack;
    use tls_openssl::x509::extension::{
        BasicConstraints, ExtendedKeyUsage, SubjectAlternativeName,
    };
    use tls_openssl::x509::{X509, X509NameBuilder};

    use super::{ExtKeyUsage, San};
    use crate::x509::Certificate;

    /// Generated private key for the platform backend.
    pub struct Key(pub(crate) PKey<Private>);

    impl Key {
        pub(super) fn ec() -> Self {
            let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap();
            Key(PKey::from_ec_key(EcKey::generate(&group).unwrap()).unwrap())
        }

        pub(super) fn rsa() -> Self {
            Key(PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap())
        }

        /// Unencrypted PKCS#8 DER private key.
        /// Exports the private key as unencrypted PKCS#8 DER.
        #[must_use]
        pub fn pkcs8_der(&self) -> Vec<u8> {
            self.0.private_key_to_pkcs8().unwrap()
        }
    }

    fn x509(cert: &Certificate) -> X509 {
        X509::from_der(cert.der()).unwrap()
    }

    pub(super) fn cert(
        cn: &str,
        ca: bool,
        issuer: Option<(&Certificate, &Key)>,
        key: &Key,
        san: &San<'_>,
        (not_before, not_after): (i64, i64),
        eku: &[ExtKeyUsage],
    ) -> Vec<u8> {
        let issuer = issuer.map(|(c, k)| (x509(c), k));
        let mut name = X509NameBuilder::new().unwrap();
        name.append_entry_by_nid(Nid::COMMONNAME, cn).unwrap();
        let name = name.build();
        let mut b = X509::builder().unwrap();
        b.set_version(2).unwrap();
        b.set_serial_number(&BigNum::from_u32(0x0a0b).unwrap().to_asn1_integer().unwrap())
            .unwrap();
        b.set_subject_name(&name).unwrap();
        b.set_issuer_name(issuer.as_ref().map_or(&name, |(c, _)| c.subject_name()))
            .unwrap();
        b.set_pubkey(&key.0).unwrap();
        b.set_not_before(&Asn1Time::from_unix(not_before).unwrap())
            .unwrap();
        b.set_not_after(&Asn1Time::from_unix(not_after).unwrap())
            .unwrap();
        if ca {
            b.append_extension(BasicConstraints::new().critical().ca().build().unwrap())
                .unwrap();
        }
        let empty = [san.dns, san.uri, san.email].iter().all(|v| v.is_empty());
        if !empty || !san.ip.is_empty() {
            let mut ext = SubjectAlternativeName::new();
            san.dns.iter().for_each(|v| _ = ext.dns(v));
            san.uri.iter().for_each(|v| _ = ext.uri(v));
            san.email.iter().for_each(|v| _ = ext.email(v));
            san.ip.iter().for_each(|v| _ = ext.ip(&v.to_string()));
            let ext = ext
                .build(&b.x509v3_context(issuer.as_ref().map(|(c, _)| c.as_ref()), None))
                .unwrap();
            b.append_extension(ext).unwrap();
        }
        if !eku.is_empty() {
            let mut ext = ExtendedKeyUsage::new();
            for usage in eku {
                match usage {
                    ExtKeyUsage::ServerAuth => _ = ext.server_auth(),
                    ExtKeyUsage::ClientAuth => _ = ext.client_auth(),
                }
            }
            b.append_extension(ext.build().unwrap()).unwrap();
        }
        b.sign(
            issuer.as_ref().map_or(&key.0, |(_, k)| &k.0),
            MessageDigest::sha256(),
        )
        .unwrap();
        b.build().to_der().unwrap()
    }

    pub(super) fn pkcs12(
        leaf: &Certificate,
        key: &Key,
        extras: &[&Certificate],
        pw: &str,
    ) -> Vec<u8> {
        let mut ca = Stack::new().unwrap();
        for cert in extras {
            ca.push(x509(cert)).unwrap();
        }
        Pkcs12::builder()
            .cert(&x509(leaf))
            .pkey(&key.0)
            .ca(ca)
            .build2(pw)
            .unwrap()
            .to_der()
            .unwrap()
    }
}

#[cfg(not(any(windows, all(unix, feature = "openssl"))))]
mod imp {
    use rcgen::{
        BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose,
        IsCa, Issuer, KeyPair, SanType, SerialNumber,
    };

    use super::{ExtKeyUsage, San};
    use crate::x509::Certificate;

    /// Generated private key for the platform backend.
    pub struct Key(KeyPair);

    impl Key {
        pub(super) fn ec() -> Self {
            Key(KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap())
        }

        pub(super) fn rsa() -> Self {
            let der =
                crate::x509::identity::pkcs8_pem(include_bytes!("../../examples/key.pem")).unwrap();
            let der = tls_rustls::pki_types::PrivatePkcs8KeyDer::from(der.as_slice());
            Key(KeyPair::from_pkcs8_der_and_sign_algo(&der, &rcgen::PKCS_RSA_SHA256).unwrap())
        }

        /// Unencrypted PKCS#8 DER private key.
        /// Exports the private key as unencrypted PKCS#8 DER.
        #[must_use]
        pub fn pkcs8_der(&self) -> Vec<u8> {
            self.0.serialize_der()
        }
    }

    fn params(cn: &str) -> CertificateParams {
        let mut params = CertificateParams::default();
        params.distinguished_name = DistinguishedName::new();
        params.distinguished_name.push(DnType::CommonName, cn);
        params
    }

    pub(super) fn cert(
        cn: &str,
        ca: bool,
        issuer: Option<(&Certificate, &Key)>,
        key: &Key,
        san: &San<'_>,
        (not_before, not_after): (i64, i64),
        eku: &[ExtKeyUsage],
    ) -> Vec<u8> {
        let time = |unix: i64| {
            rcgen::date_time_ymd(1970, 1, 1)
                + std::time::Duration::from_secs(u64::try_from(unix).unwrap())
        };
        let ia5 = |v: &&str| (*v).try_into().unwrap();
        let mut p = params(cn);
        p.serial_number = Some(SerialNumber::from(vec![0x0a, 0x0b]));
        if ca {
            p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        }
        p.subject_alt_names = san
            .dns
            .iter()
            .map(|v| SanType::DnsName(ia5(v)))
            .chain(san.uri.iter().map(|v| SanType::URI(ia5(v))))
            .chain(san.email.iter().map(|v| SanType::Rfc822Name(ia5(v))))
            .chain(san.ip.iter().map(|v| SanType::IpAddress(*v)))
            .collect();
        p.extended_key_usages = eku
            .iter()
            .map(|usage| match usage {
                ExtKeyUsage::ServerAuth => ExtendedKeyUsagePurpose::ServerAuth,
                ExtKeyUsage::ClientAuth => ExtendedKeyUsagePurpose::ClientAuth,
            })
            .collect();
        p.not_before = time(not_before);
        p.not_after = time(not_after);
        let cert = match issuer {
            None => p.self_signed(&key.0),
            Some((c, k)) => {
                let issuer = Issuer::new(params(c.subject_cn().unwrap()), &k.0);
                p.signed_by(&key.0, &issuer)
            }
        };
        cert.unwrap().der().to_vec()
    }
}

#[cfg(windows)]
mod imp {
    use std::{ffi::c_void, mem, ptr};

    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::Security::Cryptography::{
        CERT_ALT_NAME_ENTRY, CERT_ALT_NAME_ENTRY_0, CERT_ALT_NAME_INFO,
        CERT_BASIC_CONSTRAINTS2_INFO, CERT_EXTENSION, CERT_INFO, CERT_KEY_CONTEXT,
        CERT_KEY_CONTEXT_0, CERT_KEY_CONTEXT_PROP_ID, CERT_NAME_STR_FORCE_UTF8_DIR_STR_FLAG,
        CERT_NCRYPT_KEY_SPEC, CERT_PUBLIC_KEY_INFO, CERT_STORE_ADD_ALWAYS,
        CERT_STORE_NO_CRYPT_RELEASE_FLAG, CERT_STORE_PROV_MEMORY, CERT_V3, CERT_X500_NAME_STR,
        CRYPT_ALGORITHM_IDENTIFIER, CRYPT_INTEGER_BLOB, CTL_USAGE,
        CertAddEncodedCertificateToStore, CertOpenStore, CertSetCertificateContextProperty,
        CertStrToNameW, CryptEncodeObjectEx, CryptExportPublicKeyInfo,
        CryptSignAndEncodeCertificate, EXPORT_PRIVATE_KEYS, MS_KEY_STORAGE_PROVIDER,
        NCRYPT_ALLOW_EXPORT_FLAG, NCRYPT_ALLOW_PLAINTEXT_EXPORT_FLAG, NCRYPT_ECDSA_P256_ALGORITHM,
        NCRYPT_EXPORT_POLICY_PROPERTY, NCRYPT_KEY_HANDLE, NCRYPT_LENGTH_PROPERTY,
        NCRYPT_PKCS8_PRIVATE_KEY_BLOB, NCRYPT_PROV_HANDLE, NCRYPT_RSA_ALGORITHM,
        NCryptCreatePersistedKey, NCryptExportKey, NCryptFinalizeKey, NCryptFreeObject,
        NCryptOpenStorageProvider, NCryptSetProperty, PFXExportCertStoreEx, REPORT_NO_PRIVATE_KEY,
        REPORT_NOT_ABLE_TO_EXPORT_PRIVATE_KEY, X509_ALTERNATE_NAME, X509_ASN_ENCODING,
        X509_BASIC_CONSTRAINTS2, X509_CERT_TO_BE_SIGNED, X509_ENHANCED_KEY_USAGE,
        szOID_BASIC_CONSTRAINTS2, szOID_ECDSA_SHA256, szOID_ENHANCED_KEY_USAGE,
        szOID_PKIX_KP_CLIENT_AUTH, szOID_PKIX_KP_SERVER_AUTH, szOID_RSA_SHA256RSA,
        szOID_SUBJECT_ALT_NAME2,
    };
    use windows_sys::core::PCSTR;

    use super::{ExtKeyUsage, San};
    use crate::x509::Certificate;
    use crate::x509::cng::{CertCtx, Store, status, wide, win32};

    // `CERT_ALT_NAME_*` choices, not exported by windows-sys
    const ALT_RFC822: u32 = 2;
    const ALT_DNS: u32 = 3;
    const ALT_URL: u32 = 7;
    const ALT_IP: u32 = 8;

    /// Generated private key: an ephemeral, exportable CNG key and its provider.
    pub struct Key {
        provider: NCRYPT_PROV_HANDLE,
        handle: NCRYPT_KEY_HANDLE,
        rsa: bool,
    }

    impl Drop for Key {
        fn drop(&mut self) {
            // SAFETY: handles are owned; zero handles are never freed.
            unsafe {
                if self.handle != 0 {
                    NCryptFreeObject(self.handle);
                }
                NCryptFreeObject(self.provider);
            }
        }
    }

    impl Key {
        fn generate(rsa: bool) -> Self {
            let mut provider = 0;
            // SAFETY: the provider name and output pointer are valid.
            status(unsafe {
                NCryptOpenStorageProvider(&raw mut provider, MS_KEY_STORAGE_PROVIDER, 0)
            })
            .unwrap();
            let mut key = Key {
                provider,
                handle: 0,
                rsa,
            };
            let alg = if rsa {
                NCRYPT_RSA_ALGORITHM
            } else {
                NCRYPT_ECDSA_P256_ALGORITHM
            };
            // SAFETY: no key name, so the key is not persisted.
            status(unsafe {
                NCryptCreatePersistedKey(provider, &raw mut key.handle, alg, ptr::null(), 0, 0)
            })
            .unwrap();
            let set = |name, value: u32| {
                // SAFETY: the value is a readable u32.
                status(unsafe {
                    NCryptSetProperty(key.handle, name, (&raw const value).cast(), 4, 0)
                })
                .unwrap();
            };
            // tests export the key as PKCS#8 and into PKCS#12 archives
            set(
                NCRYPT_EXPORT_POLICY_PROPERTY,
                NCRYPT_ALLOW_EXPORT_FLAG | NCRYPT_ALLOW_PLAINTEXT_EXPORT_FLAG,
            );
            if rsa {
                set(NCRYPT_LENGTH_PROPERTY, 2048);
            }
            // SAFETY: the key handle is valid and not finalized.
            status(unsafe { NCryptFinalizeKey(key.handle, 0) }).unwrap();
            key
        }

        pub(super) fn ec() -> Self {
            Self::generate(false)
        }

        pub(super) fn rsa() -> Self {
            Self::generate(true)
        }

        /// Unencrypted PKCS#8 DER private key.
        /// Exports the private key as unencrypted PKCS#8 DER.
        #[must_use]
        pub fn pkcs8_der(&self) -> Vec<u8> {
            let export = |buf: &mut [u8]| {
                let mut len = 0;
                // SAFETY: the buffer is writable for its length; an empty buffer queries the size.
                status(unsafe {
                    NCryptExportKey(
                        self.handle,
                        0,
                        NCRYPT_PKCS8_PRIVATE_KEY_BLOB,
                        ptr::null(),
                        if buf.is_empty() {
                            ptr::null_mut()
                        } else {
                            buf.as_mut_ptr()
                        },
                        len32(buf),
                        &raw mut len,
                        0,
                    )
                })
                .unwrap();
                len as usize
            };
            let mut buf = vec![0; export(&mut [])];
            let len = export(&mut buf);
            buf.truncate(len);
            buf
        }
    }

    fn len32<T>(data: &[T]) -> u32 {
        u32::try_from(data.len()).unwrap()
    }

    fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB {
            cbData: len32(data),
            pbData: data.as_ptr().cast_mut(),
        }
    }

    /// Calls a Crypt32 "size, then data" function.
    fn two_pass(f: impl Fn(*mut u8, &mut u32) -> i32) -> Vec<u8> {
        let mut len = 0;
        win32(f(ptr::null_mut(), &mut len)).unwrap();
        let mut buf = vec![0; len as usize];
        win32(f(buf.as_mut_ptr(), &mut len)).unwrap();
        buf.truncate(len as usize);
        buf
    }

    fn encode(ty: PCSTR, info: *const c_void) -> Vec<u8> {
        // SAFETY: `info` points to the structure matching `ty`.
        two_pass(|buf, len| unsafe {
            CryptEncodeObjectEx(X509_ASN_ENCODING, ty, info, 0, ptr::null(), buf.cast(), len)
        })
    }

    fn name(cn: &str) -> Vec<u8> {
        // quoted, so surrounding and repeated whitespace is preserved
        let x500 = wide(&format!("CN=\"{}\"", cn.replace('"', "\"\"")));
        let flags = CERT_X500_NAME_STR | CERT_NAME_STR_FORCE_UTF8_DIR_STR_FLAG;
        // SAFETY: the name is NUL terminated.
        two_pass(|buf, len| unsafe {
            CertStrToNameW(
                X509_ASN_ENCODING,
                x500.as_ptr(),
                flags,
                ptr::null(),
                buf,
                len,
                ptr::null_mut(),
            )
        })
    }

    fn filetime(unix: i64) -> FILETIME {
        let t = u64::try_from(unix + 11_644_473_600).unwrap() * 10_000_000;
        FILETIME {
            dwLowDateTime: u32::try_from(t & 0xffff_ffff).unwrap(),
            dwHighDateTime: u32::try_from(t >> 32).unwrap(),
        }
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn cert(
        cn: &str,
        ca: bool,
        issuer: Option<(&Certificate, &Key)>,
        key: &Key,
        san: &San<'_>,
        (not_before, not_after): (i64, i64),
        eku: &[ExtKeyUsage],
    ) -> Vec<u8> {
        let subject = name(cn);
        let issuer_ctx = issuer.map(|(c, _)| CertCtx::from_der(c.der()).unwrap());
        let issuer_name = match &issuer_ctx {
            // SAFETY: the context is live, its subject is copied as raw bytes.
            Some(ctx) => unsafe { (*(*ctx.0).pCertInfo).Subject },
            None => blob(&subject),
        };

        // SubjectPublicKeyInfo with pointers into the aligned buffer
        let mut spki_len = 0;
        // SAFETY: a null buffer queries the size.
        win32(unsafe {
            CryptExportPublicKeyInfo(
                key.handle,
                CERT_NCRYPT_KEY_SPEC,
                X509_ASN_ENCODING,
                ptr::null_mut(),
                &raw mut spki_len,
            )
        })
        .unwrap();
        let mut spki = vec![0usize; (spki_len as usize).div_ceil(mem::size_of::<usize>())];
        // SAFETY: the buffer is aligned and at least `spki_len` bytes.
        win32(unsafe {
            CryptExportPublicKeyInfo(
                key.handle,
                CERT_NCRYPT_KEY_SPEC,
                X509_ASN_ENCODING,
                spki.as_mut_ptr().cast(),
                &raw mut spki_len,
            )
        })
        .unwrap();

        let texts: Vec<_> = [
            (ALT_DNS, san.dns),
            (ALT_URL, san.uri),
            (ALT_RFC822, san.email),
        ]
        .into_iter()
        .flat_map(|(choice, values)| values.iter().map(move |v| (choice, wide(v))))
        .collect();
        let ips: Vec<Vec<u8>> = san
            .ip
            .iter()
            .map(|ip| match ip {
                std::net::IpAddr::V4(a) => a.octets().to_vec(),
                std::net::IpAddr::V6(a) => a.octets().to_vec(),
            })
            .collect();
        let mut entries: Vec<_> = texts
            .iter()
            .map(|(choice, value)| CERT_ALT_NAME_ENTRY {
                dwAltNameChoice: *choice,
                // the DNS, URL and RFC822 variants share the same wide string pointer
                Anonymous: CERT_ALT_NAME_ENTRY_0 {
                    pwszDNSName: value.as_ptr().cast_mut(),
                },
            })
            .chain(ips.iter().map(|ip| CERT_ALT_NAME_ENTRY {
                dwAltNameChoice: ALT_IP,
                Anonymous: CERT_ALT_NAME_ENTRY_0 {
                    IPAddress: blob(ip),
                },
            }))
            .collect();

        let mut ext_values: Vec<(PCSTR, bool, Vec<u8>)> = Vec::new();
        if ca {
            let bc = CERT_BASIC_CONSTRAINTS2_INFO {
                fCA: 1,
                fPathLenConstraint: 0,
                dwPathLenConstraint: 0,
            };
            let value = encode(X509_BASIC_CONSTRAINTS2, (&raw const bc).cast());
            ext_values.push((szOID_BASIC_CONSTRAINTS2, true, value));
        }
        if !entries.is_empty() {
            let info = CERT_ALT_NAME_INFO {
                cAltEntry: len32(&entries),
                rgAltEntry: entries.as_mut_ptr(),
            };
            let value = encode(X509_ALTERNATE_NAME, (&raw const info).cast());
            ext_values.push((szOID_SUBJECT_ALT_NAME2, false, value));
        }
        if !eku.is_empty() {
            let mut oids: Vec<_> = eku
                .iter()
                .map(|usage| {
                    match usage {
                        ExtKeyUsage::ServerAuth => szOID_PKIX_KP_SERVER_AUTH,
                        ExtKeyUsage::ClientAuth => szOID_PKIX_KP_CLIENT_AUTH,
                    }
                    .cast_mut()
                })
                .collect();
            let usage = CTL_USAGE {
                cUsageIdentifier: len32(&oids),
                rgpszUsageIdentifier: oids.as_mut_ptr(),
            };
            let value = encode(X509_ENHANCED_KEY_USAGE, (&raw const usage).cast());
            ext_values.push((szOID_ENHANCED_KEY_USAGE, false, value));
        }
        let mut extensions: Vec<_> = ext_values
            .iter()
            .map(|(oid, critical, value)| CERT_EXTENSION {
                pszObjId: oid.cast_mut(),
                fCritical: i32::from(*critical),
                Value: blob(value),
            })
            .collect();

        let signer = issuer.map_or(key, |(_, k)| k);
        let rsa_params = [0x05, 0x00]; // ASN.1 NULL
        let alg = CRYPT_ALGORITHM_IDENTIFIER {
            pszObjId: if signer.rsa {
                szOID_RSA_SHA256RSA
            } else {
                szOID_ECDSA_SHA256
            }
            .cast_mut(),
            Parameters: blob(if signer.rsa { &rsa_params } else { &[] }),
        };
        let serial = [0x0b, 0x0a]; // little-endian 0x0a0b
        let info = CERT_INFO {
            dwVersion: CERT_V3,
            SerialNumber: blob(&serial),
            SignatureAlgorithm: alg,
            Issuer: issuer_name,
            NotBefore: filetime(not_before),
            NotAfter: filetime(not_after),
            Subject: blob(&subject),
            // SAFETY: the buffer holds a `CERT_PUBLIC_KEY_INFO` and outlives `info`.
            SubjectPublicKeyInfo: unsafe { *spki.as_ptr().cast::<CERT_PUBLIC_KEY_INFO>() },
            // SAFETY: all-zero is an absent unique id.
            IssuerUniqueId: unsafe { mem::zeroed() },
            // SAFETY: as above.
            SubjectUniqueId: unsafe { mem::zeroed() },
            cExtension: len32(&extensions),
            rgExtension: extensions.as_mut_ptr(),
        };
        // SAFETY: `info` and all buffers it points to are live for the call.
        two_pass(|buf, len| unsafe {
            CryptSignAndEncodeCertificate(
                signer.handle,
                CERT_NCRYPT_KEY_SPEC,
                X509_ASN_ENCODING,
                X509_CERT_TO_BE_SIGNED,
                (&raw const info).cast(),
                &raw const alg,
                ptr::null(),
                buf,
                len,
            )
        })
    }

    pub(super) fn pkcs12(
        leaf: &Certificate,
        key: &Key,
        extras: &[&Certificate],
        pw: &str,
    ) -> Vec<u8> {
        // SAFETY: a memory store takes no parameters.
        let store = unsafe { CertOpenStore(CERT_STORE_PROV_MEMORY, 0, 0, 0, ptr::null()) };
        assert!(!store.is_null(), "{}", std::io::Error::last_os_error());
        let store = Store(store);
        let add = |cert: &Certificate, out: *mut *mut _| {
            // SAFETY: the DER is readable; the store copies it.
            win32(unsafe {
                CertAddEncodedCertificateToStore(
                    store.0,
                    X509_ASN_ENCODING,
                    cert.der().as_ptr(),
                    len32(cert.der()),
                    CERT_STORE_ADD_ALWAYS,
                    out,
                )
            })
            .unwrap();
        };
        let mut ctx = ptr::null_mut();
        add(leaf, &raw mut ctx);
        let ctx = CertCtx(ctx);
        for cert in extras {
            add(cert, ptr::null_mut());
        }
        let key_ctx = CERT_KEY_CONTEXT {
            cbSize: u32::try_from(mem::size_of::<CERT_KEY_CONTEXT>()).unwrap(),
            Anonymous: CERT_KEY_CONTEXT_0 {
                hNCryptKey: key.handle,
            },
            dwKeySpec: CERT_NCRYPT_KEY_SPEC,
        };
        // SAFETY: the context is live; the key outlives the export and is not released by it.
        win32(unsafe {
            CertSetCertificateContextProperty(
                ctx.0,
                CERT_KEY_CONTEXT_PROP_ID,
                CERT_STORE_NO_CRYPT_RELEASE_FLAG,
                (&raw const key_ctx).cast(),
            )
        })
        .unwrap();
        let pw = wide(pw);
        let flags =
            EXPORT_PRIVATE_KEYS | REPORT_NOT_ABLE_TO_EXPORT_PRIVATE_KEY | REPORT_NO_PRIVATE_KEY;
        // SAFETY: the store is open, the password is NUL terminated and `buf` is writable
        // for `len` bytes (null queries the size).
        let pfx = two_pass(|buf, len| unsafe {
            let mut out = CRYPT_INTEGER_BLOB {
                cbData: *len,
                pbData: buf,
            };
            let ok = PFXExportCertStoreEx(store.0, &raw mut out, pw.as_ptr(), ptr::null(), flags);
            *len = out.cbData;
            ok
        });
        drop(ctx);
        pfx
    }
}
