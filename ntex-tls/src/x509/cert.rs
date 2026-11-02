//! Parsed X.509 certificate.
//!
//! Certificates are parsed with `x509-parser` (pure Rust DER parser, no crypto).
//! The DER encoding is kept as [`Bytes`] (cheap clone, zero-copy from ntex-tls) and is
//! the source for trust verification, thumbprints and PEM encoding.
//!
//! Field rules (parity with the previous OpenSSL based implementation):
//! - subject CN: first `commonName` attribute, decoded like OpenSSL `ASN1_STRING_to_UTF8`
//!   (UTF8/Printable/IA5/Numeric/Visible strings as UTF-8, T61 as Latin-1, BMP as UCS-2,
//!   Universal as UCS-4). Values that cannot be decoded are treated as missing.
//! - SAN DNS / URI / email: values are kept as encoded; entries with invalid encoding
//!   are skipped. A certificate with a malformed or duplicated SAN extension has no SANs.
//! - SAN IP: raw octets are kept; [`SanIp::addr`] returns `None` unless 4 or 16 bytes.
//! - case folding is done by callers (see [`crate::x509::auth`]), values are returned as encoded.
//! - thumbprints are computed lazily over the DER encoding and rendered as lowercase hex.
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime};
use std::{fmt, ops::Range};

use base64::Engine;
use ntex_bytes::Bytes;
use x509_parser::asn1_rs::{Any, Tag};
use x509_parser::extensions::GeneralName;
use x509_parser::prelude::{FromDer, X509Certificate};
use x509_parser::time::ASN1Time;

use crate::x509::{BASE64, Error, digest};

const PEM_BEGIN: &str = "-----BEGIN ";
const PEM_END: &str = "-----END ";
const PEM_LABELS: [&str; 2] = ["CERTIFICATE", "X509 CERTIFICATE"];

/// Parsed X.509 certificate. Cheap to clone (`Arc`), `Send + Sync`.
#[derive(Clone)]
pub struct Certificate(Arc<Inner>);

struct Inner {
    der: Bytes,
    spki: Range<usize>,
    subject_raw: Range<usize>,
    issuer_raw: Range<usize>,
    key_type: Option<KeyType>,
    version: u32,
    serial: String,
    subject: String,
    issuer: String,
    subject_cn: Option<String>,
    san: SubjectAltNames,
    not_before: SystemTime,
    not_after: SystemTime,
    sha1: OnceLock<[u8; digest::SHA1_LEN]>,
    sha256: OnceLock<[u8; digest::SHA256_LEN]>,
}

/// Subject alternative names of a certificate.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubjectAltNames {
    /// `dNSName` entries.
    pub dns: Vec<String>,
    /// `uniformResourceIdentifier` entries.
    pub uri: Vec<String>,
    /// `rfc822Name` entries.
    pub email: Vec<String>,
    /// `iPAddress` entries.
    pub ip: Vec<SanIp>,
}

/// Public key type of a certificate, see [`Certificate::key_type`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyType {
    /// `rsaEncryption`.
    Rsa,
    /// `id-ecPublicKey` on `prime256v1`.
    EcP256,
    /// `id-ecPublicKey` on `secp384r1`.
    EcP384,
    /// `id-ecPublicKey` on `secp521r1`.
    EcP521,
}

/// Raw `iPAddress` SAN entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SanIp(pub Vec<u8>);

impl SanIp {
    /// Address for 4 (IPv4) or 16 (IPv6) octets, `None` for malformed entries.
    #[must_use]
    pub fn addr(&self) -> Option<IpAddr> {
        if let Ok(b) = <[u8; 4]>::try_from(self.0.as_slice()) {
            Some(IpAddr::V4(Ipv4Addr::from(b)))
        } else if let Ok(b) = <[u8; 16]>::try_from(self.0.as_slice()) {
            Some(IpAddr::V6(Ipv6Addr::from(b)))
        } else {
            None
        }
    }
}

impl Certificate {
    /// Parses a DER encoded certificate, taking ownership of the buffer (no copy).
    ///
    /// Trailing data after the certificate is ignored (and dropped from [`Self::der`]).
    ///
    /// # Errors
    /// [`Error::InvalidDer`] if the data is not a valid X.509 certificate, its version is
    /// unsupported, or its validity dates cannot be represented by [`SystemTime`].
    pub fn from_der(der: impl Into<Bytes>) -> Result<Self, Error> {
        let mut der = der.into();
        let (rest, cert) =
            X509Certificate::from_der(&der).map_err(|e| Error::InvalidDer(e.to_string()))?;
        let len = der.len() - rest.len();
        let version = cert.version().0;
        if version > 2 {
            return Err(Error::InvalidDer(format!(
                "unsupported X.509 version: {version}"
            )));
        }

        let spki = cert.tbs_certificate.subject_pki.raw;
        let spki_start = spki.as_ptr() as usize - der.as_ptr() as usize;
        let spki = spki_start..spki_start + spki.len();
        let range = |raw: &[u8]| {
            let start = raw.as_ptr() as usize - der.as_ptr() as usize;
            start..start + raw.len()
        };
        let subject_raw = range(cert.subject().as_raw());
        let issuer_raw = range(cert.issuer().as_raw());
        let key_type = key_type(cert.public_key());

        let subject_cn = cert
            .subject()
            .iter_common_name()
            .next()
            .and_then(|cn| decode_string(cn.attr_value()));

        let mut san = SubjectAltNames::default();
        if let Ok(Some(ext)) = cert.subject_alternative_name() {
            for name in &ext.value.general_names {
                match name {
                    GeneralName::DNSName(v) => san.dns.push((*v).to_string()),
                    GeneralName::URI(v) => san.uri.push((*v).to_string()),
                    GeneralName::RFC822Name(v) => san.email.push((*v).to_string()),
                    GeneralName::IPAddress(v) => san.ip.push(SanIp(v.to_vec())),
                    _ => {}
                }
            }
        }

        let inner = Inner {
            spki,
            subject_raw,
            issuer_raw,
            key_type,
            version: version + 1,
            serial: serial_hex(cert.raw_serial()),
            subject: cert.subject().to_string(),
            issuer: cert.issuer().to_string(),
            subject_cn,
            san,
            not_before: to_system_time(cert.validity().not_before)?,
            not_after: to_system_time(cert.validity().not_after)?,
            sha1: OnceLock::new(),
            sha256: OnceLock::new(),
            der: Bytes::new(),
        };
        drop(cert);
        der.truncate(len);
        Ok(Certificate(Arc::new(Inner { der, ..inner })))
    }

    /// Parses the first `CERTIFICATE` (or `X509 CERTIFICATE`) block of a PEM document.
    ///
    /// # Errors
    /// [`Error::InvalidPem`] if no certificate block is found or base64 is invalid,
    /// [`Error::InvalidDer`] if the decoded block is not a valid certificate.
    pub fn from_pem(pem: &[u8]) -> Result<Self, Error> {
        let der = pem_blocks(pem)
            .next()
            .ok_or_else(|| Error::InvalidPem("no CERTIFICATE block found".into()))??;
        Self::from_der(der)
    }

    /// Parses all `CERTIFICATE` blocks of a PEM document, in order.
    ///
    /// # Errors
    /// See [`Self::from_pem`]. An empty document returns an empty list.
    pub fn from_pem_multi(pem: &[u8]) -> Result<Vec<Self>, Error> {
        pem_blocks(pem)
            .map(|der| der.and_then(Self::from_der))
            .collect()
    }

    /// DER encoding.
    #[must_use]
    pub fn der(&self) -> &Bytes {
        &self.0.der
    }

    /// DER encoded `SubjectPublicKeyInfo`.
    #[must_use]
    pub fn public_key_der(&self) -> &[u8] {
        &self.0.der[self.0.spki.clone()]
    }

    /// X.509 version (1, 2 or 3).
    #[must_use]
    pub fn version(&self) -> u32 {
        self.0.version
    }

    /// Serial number, uppercase hex without leading zero bytes (OpenSSL `BN_bn2hex` style).
    #[must_use]
    pub fn serial_hex(&self) -> &str {
        &self.0.serial
    }

    /// Subject distinguished name (RFC 4514 style), for logging.
    #[must_use]
    pub fn subject(&self) -> &str {
        &self.0.subject
    }

    /// Issuer distinguished name (RFC 4514 style), for logging.
    #[must_use]
    pub fn issuer(&self) -> &str {
        &self.0.issuer
    }

    /// DER encoded subject name.
    #[must_use]
    pub fn subject_der(&self) -> &[u8] {
        &self.0.der[self.0.subject_raw.clone()]
    }

    /// DER encoded issuer name.
    #[must_use]
    pub fn issuer_der(&self) -> &[u8] {
        &self.0.der[self.0.issuer_raw.clone()]
    }

    /// Whether subject and issuer names are equal (byte comparison of the DER names); the
    /// signature is not checked.
    #[must_use]
    pub fn is_self_issued(&self) -> bool {
        self.subject_der() == self.issuer_der()
    }

    /// Public key type, `None` for unsupported key types or curves.
    #[must_use]
    pub fn key_type(&self) -> Option<KeyType> {
        self.0.key_type
    }

    /// First subject `commonName`, as encoded (no case folding).
    #[must_use]
    pub fn subject_cn(&self) -> Option<&str> {
        self.0.subject_cn.as_deref()
    }

    /// Subject alternative names.
    #[must_use]
    pub fn san(&self) -> &SubjectAltNames {
        &self.0.san
    }

    /// Start of the validity period.
    #[must_use]
    pub fn not_before(&self) -> SystemTime {
        self.0.not_before
    }

    /// End of the validity period.
    #[must_use]
    pub fn not_after(&self) -> SystemTime {
        self.0.not_after
    }

    /// SHA-1 digest of the DER encoding (computed once).
    #[must_use]
    pub fn sha1(&self) -> &[u8; digest::SHA1_LEN] {
        self.0.sha1.get_or_init(|| digest::sha1(&self.0.der))
    }

    /// SHA-256 digest of the DER encoding (computed once).
    #[must_use]
    pub fn sha256(&self) -> &[u8; digest::SHA256_LEN] {
        self.0.sha256.get_or_init(|| digest::sha256(&self.0.der))
    }

    /// Digest of the DER encoding (computed once per algorithm).
    #[must_use]
    pub fn digest(&self, alg: digest::Algorithm) -> &[u8] {
        match alg {
            digest::Algorithm::Sha1 => self.sha1(),
            digest::Algorithm::Sha256 => self.sha256(),
        }
    }

    /// Thumbprint as lowercase hex.
    #[must_use]
    pub fn thumbprint(&self, alg: digest::Algorithm) -> String {
        hex::encode(self.digest(alg))
    }

    /// PEM encoding (`CERTIFICATE` label, 64 column lines, trailing newline).
    #[must_use]
    pub fn to_pem(&self) -> String {
        pem_encode("CERTIFICATE", &self.0.der)
    }
}

/// PEM encodes `der` with `label` (64 column lines, trailing newline).
fn key_type(spki: &x509_parser::x509::SubjectPublicKeyInfo<'_>) -> Option<KeyType> {
    use x509_parser::oid_registry::{
        OID_EC_P256, OID_KEY_TYPE_EC_PUBLIC_KEY, OID_NIST_EC_P384, OID_NIST_EC_P521,
        OID_PKCS1_RSAENCRYPTION,
    };

    let alg = &spki.algorithm;
    if alg.algorithm == OID_PKCS1_RSAENCRYPTION {
        Some(KeyType::Rsa)
    } else if alg.algorithm == OID_KEY_TYPE_EC_PUBLIC_KEY {
        let curve = alg.parameters.as_ref()?.as_oid().ok()?;
        if curve == OID_EC_P256 {
            Some(KeyType::EcP256)
        } else if curve == OID_NIST_EC_P384 {
            Some(KeyType::EcP384)
        } else if curve == OID_NIST_EC_P521 {
            Some(KeyType::EcP521)
        } else {
            None
        }
    } else {
        None
    }
}

/// Encodes DER as PEM with the given `label` (64 column base64 lines, trailing newline).
#[must_use]
pub fn pem_encode(label: &str, der: &[u8]) -> String {
    let b64 = BASE64.encode(der);
    let mut pem = String::with_capacity(b64.len() + b64.len() / 64 + 2 * label.len() + 32);
    pem.push_str("-----BEGIN ");
    pem.push_str(label);
    pem.push_str("-----\n");
    for line in b64.as_bytes().chunks(64) {
        // base64 output is ASCII
        pem.push_str(std::str::from_utf8(line).unwrap_or_default());
        pem.push('\n');
    }
    pem.push_str("-----END ");
    pem.push_str(label);
    pem.push_str("-----\n");
    pem
}

impl PartialEq for Certificate {
    fn eq(&self, other: &Self) -> bool {
        self.0.der == other.0.der
    }
}

impl Eq for Certificate {}

impl fmt::Debug for Certificate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut d = f.debug_struct("X509");
        d.field("serial_number", &self.0.serial)
            .field("issuer", &self.0.issuer)
            .field("subject", &self.0.subject);
        let san = &self.0.san;
        if !(san.dns.is_empty() && san.uri.is_empty() && san.email.is_empty() && san.ip.is_empty())
        {
            let ips: Vec<_> = san
                .ip
                .iter()
                .map(|ip| {
                    ip.addr()
                        .map_or_else(|| hex::encode(&ip.0), |a| a.to_string())
                })
                .collect();
            d.field("san_dns", &san.dns)
                .field("san_uri", &san.uri)
                .field("san_email", &san.email)
                .field("san_ip", &ips);
        }
        d.field("not_before", &DisplayTime(self.0.not_before))
            .field("not_after", &DisplayTime(self.0.not_after))
            .finish()
    }
}

struct DisplayTime(SystemTime);

impl fmt::Debug for DisplayTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ts = match self.0.duration_since(SystemTime::UNIX_EPOCH) {
            Ok(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
            Err(e) => -i64::try_from(e.duration().as_secs()).unwrap_or(i64::MAX),
        };
        match ASN1Time::from_timestamp(ts) {
            Ok(t) => write!(f, "{t}"),
            Err(_) => write!(f, "{ts}"),
        }
    }
}

fn to_system_time(t: ASN1Time) -> Result<SystemTime, Error> {
    let ts = t.timestamp();
    if ts >= 0 {
        SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(ts.unsigned_abs()))
    } else {
        SystemTime::UNIX_EPOCH.checked_sub(Duration::from_secs(ts.unsigned_abs()))
    }
    .ok_or_else(|| Error::InvalidDer(format!("certificate time is out of range: {t}")))
}

fn serial_hex(raw: &[u8]) -> String {
    let start = raw.iter().position(|b| *b != 0).unwrap_or(raw.len());
    if start == raw.len() {
        "0".to_string()
    } else {
        hex::encode_upper(&raw[start..])
    }
}

/// Decodes an ASN.1 string like OpenSSL `ASN1_STRING_to_UTF8`.
fn decode_string(value: &Any<'_>) -> Option<String> {
    let data = value.data;
    match value.tag() {
        Tag::Utf8String
        | Tag::PrintableString
        | Tag::Ia5String
        | Tag::NumericString
        | Tag::VisibleString => std::str::from_utf8(data).ok().map(ToString::to_string),
        // OpenSSL treats T61String as Latin-1
        Tag::T61String => Some(data.iter().map(|b| char::from(*b)).collect()),
        Tag::BmpString => {
            if !data.len().is_multiple_of(2) {
                return None;
            }
            char::decode_utf16(
                data.as_chunks::<2>()
                    .0
                    .iter()
                    .map(|&c| u16::from_be_bytes(c)),
            )
            .collect::<Result<String, _>>()
            .ok()
        }
        Tag::UniversalString => {
            if !data.len().is_multiple_of(4) {
                return None;
            }
            data.as_chunks::<4>()
                .0
                .iter()
                .map(|&c| char::from_u32(u32::from_be_bytes(c)))
                .collect()
        }
        _ => None,
    }
}

/// Iterates over DER payloads of certificate PEM blocks.
fn pem_blocks(pem: &[u8]) -> impl Iterator<Item = Result<Bytes, Error>> + '_ {
    let text = String::from_utf8_lossy(pem);
    let mut blocks = Vec::new();
    let mut rest: &str = &text;
    while let Some(start) = rest.find(PEM_BEGIN) {
        let after = &rest[start + PEM_BEGIN.len()..];
        let Some(label_end) = after.find("-----") else {
            blocks.push(Err(Error::InvalidPem("malformed BEGIN line".into())));
            break;
        };
        let label = &after[..label_end];
        let body = &after[label_end + 5..];
        let end_marker = format!("{PEM_END}{label}-----");
        let Some(end) = body.find(&end_marker) else {
            blocks.push(Err(Error::InvalidPem(format!(
                "missing END line for {label}"
            ))));
            break;
        };
        if PEM_LABELS.contains(&label) {
            let b64: Vec<u8> = body[..end]
                .bytes()
                .filter(|b| !b.is_ascii_whitespace())
                .collect();
            blocks.push(
                BASE64
                    .decode(&b64)
                    .map(Bytes::from)
                    .map_err(|e| Error::InvalidPem(e.to_string())),
            );
        }
        rest = &body[end + end_marker.len()..];
    }
    blocks.into_iter()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    #[cfg(all(unix, feature = "openssl"))]
    use crate::x509::digest::Algorithm;

    use crate::x509::testgen::{self, San};

    fn sample() -> Certificate {
        let ips = ["127.0.0.1".parse().unwrap(), "::1".parse().unwrap()];
        let san = San {
            dns: &["Host.Example"],
            uri: &["https://Uri"],
            email: &["A@b.c"],
            ip: &ips,
        };
        testgen::cert("Client1", false, None, &testgen::key(), &san, (-1, 1))
    }

    /// STATEMENT: Certificate metadata is parsed on every backend.
    ///
    /// HOW:
    /// - Generate a certificate with multiple SAN kinds with the platform generator.
    /// - Check version, serial, names, SANs, validity order and the debug output.
    #[test]
    fn metadata() {
        let c = sample();
        assert_eq!(c.version(), 3);
        assert_eq!(c.serial_hex(), "0A0B");
        assert_eq!(c.subject_cn(), Some("Client1"));
        assert_eq!(c.subject(), "CN=Client1");
        assert_eq!(c.issuer(), "CN=Client1");
        assert!(c.is_self_issued());
        assert_eq!(c.san().dns, vec!["Host.Example"]);
        assert_eq!(c.san().uri, vec!["https://Uri"]);
        assert_eq!(c.san().email, vec!["A@b.c"]);
        let ips: Vec<_> = c
            .san()
            .ip
            .iter()
            .map(|i| i.addr().unwrap().to_string())
            .collect();
        assert_eq!(ips, vec!["127.0.0.1", "::1"]);
        assert!(c.not_before() < SystemTime::now());
        assert!(c.not_before() < c.not_after());
        let dbg = format!("{c:?}");
        assert!(dbg.contains("serial_number: \"0A0B\""), "{dbg}");
        assert!(dbg.contains("127.0.0.1"), "{dbg}");
    }

    /// STATEMENT: Parsed certificate digests and encodings agree with OpenSSL.
    ///
    /// HOW:
    /// - Compare digests, public key, PEM encoding, and validity of a generated certificate
    ///   against OpenSSL.
    #[cfg(all(unix, feature = "openssl"))]
    #[test]
    fn parity_with_openssl() {
        let c = sample();
        let x = tls_openssl::x509::X509::from_der(c.der()).unwrap();
        assert_eq!(c.sha1(), &tls_openssl::sha::sha1(&x.to_der().unwrap()));
        assert_eq!(
            c.thumbprint(Algorithm::Sha1),
            hex::encode(x.digest(tls_openssl::hash::MessageDigest::sha1()).unwrap())
        );
        assert_eq!(
            c.thumbprint(Algorithm::Sha256),
            hex::encode(
                x.digest(tls_openssl::hash::MessageDigest::sha256())
                    .unwrap()
            )
        );
        assert_eq!(c.to_pem().as_bytes(), x.to_pem().unwrap());
        assert_eq!(
            c.public_key_der(),
            x.public_key().unwrap().public_key_to_der().unwrap()
        );
        let epoch = tls_openssl::asn1::Asn1Time::from_unix(0).unwrap();
        let d = epoch.diff(x.not_after()).unwrap();
        let secs = u64::try_from(d.days).unwrap() * 86400 + u64::try_from(d.secs).unwrap();
        assert_eq!(
            c.not_after(),
            SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
        );
    }

    /// STATEMENT: Certificate parsing supports PEM bundles and excludes trailing DER data.
    ///
    /// HOW:
    /// - Round-trip a generated certificate, parse a bundle, and check malformed inputs fail.
    /// - Append bytes to DER and check the parsed certificate retains only its encoding.
    #[test]
    fn pem_roundtrip_and_trailing_data() {
        let c = sample();
        let mut doc = b"junk\n".to_vec();
        doc.extend_from_slice(c.to_pem().as_bytes());
        doc.extend_from_slice(c.to_pem().as_bytes());
        assert_eq!(Certificate::from_pem(&doc).unwrap(), c);
        assert_eq!(Certificate::from_pem_multi(&doc).unwrap().len(), 2);
        assert_eq!(
            Certificate::from_pem_multi(b"").unwrap(),
            Vec::<Certificate>::new()
        );
        assert!(matches!(
            Certificate::from_pem(b"nothing"),
            Err(Error::InvalidPem(_))
        ));
        assert!(matches!(
            Certificate::from_pem(b"-----BEGIN CERTIFICATE-----\n!!!\n-----END CERTIFICATE-----\n"),
            Err(Error::InvalidPem(_))
        ));

        let mut der = c.der().to_vec();
        let len = der.len();
        der.extend_from_slice(b"trailing");
        let c2 = Certificate::from_der(der).unwrap();
        assert_eq!(c2.der().len(), len);
        assert_eq!(c2, c);
        assert!(matches!(
            Certificate::from_der(&b"garbage"[..]),
            Err(Error::InvalidDer(_))
        ));
    }

    fn sequence(body: &[u8]) -> Vec<u8> {
        let mut der = vec![0x30];
        if body.len() < 128 {
            der.push(u8::try_from(body.len()).unwrap());
        } else {
            let len = body.len().to_be_bytes();
            let start = len.iter().position(|b| *b != 0).unwrap();
            der.push(0x80 | u8::try_from(len.len() - start).unwrap());
            der.extend_from_slice(&len[start..]);
        }
        der.extend_from_slice(body);
        der
    }

    fn edit_tbs(edit: impl FnOnce(&[u8]) -> Vec<u8>) -> Vec<u8> {
        let cert = Certificate::from_pem(include_bytes!("../../tests/testdata/root.pem")).unwrap();
        let (_, outer) = Any::from_der(cert.der()).unwrap();
        let (rest, tbs) = Any::from_der(outer.data).unwrap();
        let mut body = sequence(&edit(tbs.data));
        body.extend_from_slice(rest);
        sequence(&body)
    }

    /// STATEMENT: Unsupported certificate versions return `InvalidDer` without overflowing.
    ///
    /// HOW:
    /// - Replace the fixture's version with 3 and `u32::MAX` while preserving valid DER.
    /// - Confirm the underlying parser accepts the encodings but `Certificate` rejects them.
    #[test]
    fn unsupported_versions() {
        for version in [
            &[0xa0, 3, 2, 1, 3][..],
            &[0xa0, 7, 2, 5, 0, 0xff, 0xff, 0xff, 0xff][..],
        ] {
            let der = edit_tbs(|tbs| {
                let (rest, _) = Any::from_der(tbs).unwrap();
                let mut body = version.to_vec();
                body.extend_from_slice(rest);
                body
            });
            assert!(X509Certificate::from_der(&der).is_ok());
            assert!(matches!(
                Certificate::from_der(der),
                Err(Error::InvalidDer(_))
            ));
        }
    }

    /// STATEMENT: Certificate dates outside the platform's `SystemTime` range return `InvalidDer`.
    ///
    /// HOW:
    /// - Replace the fixture's `notBefore` with dates in 1500, 1601, and 1960.
    /// - Check pre-1601 dates are rejected on Windows and representable dates retain their value.
    #[test]
    fn validity_time_range() {
        for date in [b"15000101000000Z", b"16010101000000Z", b"19600101000000Z"] {
            let der = edit_tbs(|tbs| {
                let mut rest = tbs;
                for _ in 0..4 {
                    rest = Any::from_der(rest).unwrap().0;
                }
                let prefix = &tbs[..tbs.len() - rest.len()];
                let (suffix, validity) = Any::from_der(rest).unwrap();
                let (not_after, _) = Any::from_der(validity.data).unwrap();
                let mut times = vec![0x18, 15];
                times.extend_from_slice(date);
                times.extend_from_slice(not_after);
                let mut body = prefix.to_vec();
                body.extend_from_slice(&sequence(&times));
                body.extend_from_slice(suffix);
                body
            });
            let (_, parsed) = X509Certificate::from_der(&der).unwrap();
            let seconds = parsed.validity().not_before.timestamp().unsigned_abs();
            let result = Certificate::from_der(der);
            if cfg!(windows) && date == b"15000101000000Z" {
                assert!(matches!(result, Err(Error::InvalidDer(_))));
            } else {
                assert_eq!(
                    result.unwrap().not_before(),
                    SystemTime::UNIX_EPOCH - Duration::from_secs(seconds)
                );
            }
        }
    }

    /// STATEMENT: ASN.1 string decoding handles supported encodings and rejects malformed values.
    ///
    /// HOW:
    /// - Decode BMP, T61, and Universal strings and reject malformed or unsupported values.
    /// - Check malformed SAN IPs, zero serials, and pre-epoch time conversion.
    #[test]
    fn string_decoding() {
        let any = |tag, data: &'static [u8]| Any::from_tag_and_data(tag, data);
        assert_eq!(
            decode_string(&any(Tag::BmpString, b"\x00A\x00b")).as_deref(),
            Some("Ab")
        );
        assert_eq!(
            decode_string(&any(Tag::T61String, b"\xe9")).as_deref(),
            Some("\u{e9}")
        );
        assert_eq!(
            decode_string(&any(Tag::UniversalString, b"\x00\x00\x00A")).as_deref(),
            Some("A")
        );
        assert_eq!(decode_string(&any(Tag::BmpString, b"\x00")), None);
        assert_eq!(decode_string(&any(Tag::Integer, b"\x01")), None);
        assert_eq!(SanIp(vec![1, 2, 3]).addr(), None);
        assert_eq!(serial_hex(&[0, 0]), "0");
        assert_eq!(
            to_system_time(ASN1Time::from_timestamp(-10).unwrap()).unwrap(),
            SystemTime::UNIX_EPOCH - Duration::from_secs(10)
        );
    }
}
