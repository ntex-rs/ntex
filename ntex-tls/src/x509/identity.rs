//! Server/client identity (certificate + private key) loading.
//!
//! PEM/DER loading accepts a leaf-first certificate list and an unencrypted PKCS#8 key.
//! The key must match the leaf. Loading does not establish certificate trust.
//!
//! PKCS#12 uses native Windows import or, on Unix, the `openssl` feature, independently
//! of the selected trust backend. Without that capability it returns [`Error::Unsupported`].
//! System certificate stores are available only on Windows.
//!
//! [`IdentitySource`] is parsed from the existing string configuration values: a plain value
//! is a PKCS#12 file path (empty password), a value starting with `cert:` selects a Windows
//! system store, see [`IdentitySource::from_str`](std::str::FromStr). Store sources are
//! rejected when parsed on non-Windows targets.
//!
//! Store selection semantics:
//! - the existing store is opened read-only, a missing store is not created;
//! - by thumbprint (40 hex chars, SHA-1 of DER): validity dates are ignored;
//! - by subject: case-insensitive substring of the subject attribute values joined by `", "`
//!   (attribute names such as `CN=` do not match), only currently valid certificates with a
//!   private key are considered and the one that expires last wins;
//! - no chain trust validation is performed, the private key must be accessible to the
//!   service account.
//!
//! [`PrivateKey`] supports signing on every backend and fallible PKCS#8 export. On Windows,
//! keys remain in CNG. [`Identity`] imports persist an isolated key for Schannel compatibility
//! and delete it on final drop; abnormal exits may leave the temporary key behind.
//! [`ClientIdentity`] imports use ephemeral keys instead.
//!
//! Key material is never logged, `Debug` redacts the key.
//!
//! # Client identity
//!
//! [`ClientIdentity`] is the identity presented by outbound TLS clients (rustls). Its chain is
//! leaf first, ordered by issuer, without a self-signed root.
//!
//! Chain ordering checks issuer metadata and signatures, but does not check trust or dates.
//! Rustls adapters are provided in `crate::rustls` when that feature is enabled.
use std::{fmt, str::FromStr};

use base64::Engine;

use crate::x509::{BASE64, Certificate, Error, PrivateKey, crypto};

#[cfg(windows)]
pub use crate::x509::cng::CngKey;
pub use crate::x509::key::{HashAlg, SignScheme};

/// Location of a Windows system certificate store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreLocation {
    /// `Cert:\CurrentUser`
    CurrentUser,
    /// `Cert:\LocalMachine`
    LocalMachine,
}

/// Certificate selection within a Windows system certificate store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreSelector {
    /// SHA-1 thumbprint of the DER certificate.
    Thumbprint([u8; 20]),
    /// Subject attribute value substring, see the module docs.
    Subject(String),
}

/// Where an identity is loaded from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentitySource {
    /// PKCS#12 file with an empty password.
    Pkcs12File(String),
    /// PEM certificate chain and unencrypted PKCS#8 private-key files.
    PemFiles {
        /// Leaf-first PEM certificate chain.
        certificates: String,
        /// PEM `PRIVATE KEY` file; loaded into a zero-on-drop buffer.
        private_key: String,
    },
    /// Windows system certificate store, e.g. `cert:LocalMachine\MY\<thumbprint>`.
    Store {
        location: StoreLocation,
        store: String,
        select: StoreSelector,
    },
}

impl FromStr for IdentitySource {
    type Err = Error;

    /// Parses a configuration value.
    ///
    /// - `cert:<CurrentUser|LocalMachine>\<store>\<40 hex thumbprint>`
    /// - `cert:<CurrentUser|LocalMachine>\<store>\subject=<subject>`
    /// - anything else is a PKCS#12 file path.
    ///
    /// `/` may be used instead of `\`; `cert:` and the location are case-insensitive.
    ///
    /// # Errors
    /// [`Error::Unsupported`] for store sources on non-Windows targets,
    /// [`Error::InvalidIdentity`] for malformed store sources.
    fn from_str(s: &str) -> Result<Self, Error> {
        let Some(rest) = s
            .get(..5)
            .filter(|p| p.eq_ignore_ascii_case("cert:"))
            .map(|_| &s[5..])
        else {
            return Ok(IdentitySource::Pkcs12File(s.to_string()));
        };
        if cfg!(not(windows)) {
            return Err(Error::Unsupported(format!(
                "certificate store identity is Windows only: {s}"
            )));
        }
        let invalid = || {
            Error::InvalidIdentity(format!(
                "expected cert:<Location>\\<Store>\\<thumbprint|subject=..>: {s}"
            ))
        };

        let mut parts = rest.trim_start_matches(['\\', '/']).splitn(3, ['\\', '/']);
        let (Some(location), Some(store), Some(select)) =
            (parts.next(), parts.next(), parts.next())
        else {
            return Err(invalid());
        };
        let location = if location.eq_ignore_ascii_case("CurrentUser") {
            StoreLocation::CurrentUser
        } else if location.eq_ignore_ascii_case("LocalMachine") {
            StoreLocation::LocalMachine
        } else {
            return Err(invalid());
        };
        if store.is_empty() {
            return Err(invalid());
        }
        let select = if let Some(subject) = select.strip_prefix("subject=") {
            if subject.is_empty() {
                return Err(invalid());
            }
            StoreSelector::Subject(subject.to_string())
        } else {
            let mut tp = [0u8; 20];
            hex::decode_to_slice(select, &mut tp).map_err(|_| invalid())?;
            StoreSelector::Thumbprint(tp)
        };
        Ok(IdentitySource::Store {
            location,
            store: store.to_string(),
            select,
        })
    }
}

impl IdentitySource {
    /// Loads the identity.
    ///
    /// # Errors
    /// [`Error::NotFound`] if the file cannot be read, otherwise see [`Identity::from_pkcs12`]
    /// or [`Identity::from_pem`]. Store loading is Windows-only.
    pub fn load(&self) -> Result<Identity, Error> {
        match self {
            IdentitySource::Pkcs12File(path) => {
                let der =
                    std::fs::read(path).map_err(|e| Error::NotFound(format!("{path}: {e}")))?;
                Identity::from_pkcs12(&der, "")
            }
            IdentitySource::PemFiles {
                certificates,
                private_key,
            } => {
                let certs = std::fs::read(certificates)
                    .map_err(|e| Error::NotFound(format!("{certificates}: {e}")))?;
                let key = zeroize::Zeroizing::new(
                    std::fs::read(private_key)
                        .map_err(|e| Error::NotFound(format!("{private_key}: {e}")))?,
                );
                Identity::from_pem(&certs, &key)
            }
            IdentitySource::Store {
                location,
                store,
                select,
            } => Identity::from_store(*location, store, select),
        }
    }
}

/// Certificate with its private key, see the module docs.
#[derive(Clone)]
pub struct Identity {
    cert: Certificate,
    chain: Vec<Certificate>,
    key: PrivateKey,
    #[cfg(windows)]
    schannel: crate::schannel::Certificate,
}

impl Identity {
    /// Loads a leaf-first DER certificate list and an unencrypted PKCS#8 private key.
    ///
    /// # Errors
    /// Rejects empty chains, invalid certificates/keys, mismatched keys and unsupported key
    /// algorithms. On the rustls backend, install a provider or enable `rustls-ring`.
    pub fn from_der(certificates: &[&[u8]], private_key: &[u8]) -> Result<Self, Error> {
        let certs = certificates
            .iter()
            .map(|der| Certificate::from_der(ntex_bytes::Bytes::copy_from_slice(der)))
            .collect::<Result<_, _>>()?;
        Self::from_certificates(certs, private_key)
    }

    /// Loads PEM certificates and one unencrypted `PRIVATE KEY` (PKCS#8) block.
    ///
    /// The two slices may reference the same combined PEM document.
    ///
    /// # Errors
    /// See [`Self::from_der`]; malformed PEM or multiple private keys are rejected.
    pub fn from_pem(certificates: &[u8], private_key: &[u8]) -> Result<Self, Error> {
        let key = pkcs8_pem(private_key)?;
        Self::from_certificates(Certificate::from_pem_multi(certificates)?, &key)
    }

    fn from_certificates(mut certs: Vec<Certificate>, key: &[u8]) -> Result<Self, Error> {
        if certs.is_empty() {
            return Err(Error::InvalidIdentity("Certificate is missing".into()));
        }
        let private_key = PrivateKey::from_pkcs8(&certs[0], key)?;
        #[cfg(not(windows))]
        {
            let cert = certs.remove(0);
            Ok(Self {
                cert,
                chain: certs,
                key: private_key,
            })
        }
        #[cfg(windows)]
        {
            drop(private_key);
            let schannel = crate::schannel::Certificate::from_pkcs8(&certs, key)
                .map_err(|e| Error::InvalidIdentity(e.to_string()))?;
            // The input remains the source of additional certificates, not the system stores.
            let mut identity = Self::from_schannel(schannel)?;
            certs.remove(0);
            identity.chain = certs;
            Ok(identity)
        }
    }

    /// Loads an identity from a DER encoded PKCS#12 archive.
    ///
    /// # Errors
    /// [`Error::InvalidDer`] if the input is not a PKCS#12 archive or a certificate is not valid
    /// DER;
    /// [`Error::InvalidIdentity`] if the archive cannot be decrypted with `password` or has no
    /// certificate/private key (on Windows also for malformed archive contents);
    /// [`Error::Unsupported`] outside Windows unless Unix and `openssl` are enabled.
    pub fn from_pkcs12(der: &[u8], password: &str) -> Result<Self, Error> {
        #[cfg(all(unix, feature = "openssl"))]
        {
            let invalid = |e: tls_openssl::error::ErrorStack| Error::InvalidIdentity(e.to_string());
            let parsed = tls_openssl::pkcs12::Pkcs12::from_der(der)
                .map_err(|e| Error::InvalidDer(e.to_string()))?
                .parse2(password)
                .map_err(invalid)?;
            let cert = parsed
                .cert
                .ok_or_else(|| Error::InvalidIdentity("Certificate is missing".into()))?;
            let key = parsed
                .pkey
                .ok_or_else(|| Error::InvalidIdentity("Private key is missing".into()))?;
            let key = zeroize::Zeroizing::new(key.private_key_to_pkcs8().map_err(invalid)?);
            let mut chain = vec![Certificate::from_der(cert.to_der().map_err(invalid)?)?];
            chain.extend(
                parsed
                    .ca
                    .into_iter()
                    .flatten()
                    .map(|c| Certificate::from_der(c.to_der().map_err(invalid)?))
                    .collect::<Result<Vec<_>, _>>()?,
            );
            Self::from_certificates(chain, &key)
        }
        #[cfg(windows)]
        {
            crate::x509::cng::pfx_blob(der)?;
            let schannel = crate::schannel::Certificate::from_pkcs12(der, password)
                .map_err(|e| Error::InvalidIdentity(e.to_string()))?;
            Self::from_schannel(schannel)
        }
        #[cfg(not(any(windows, all(unix, feature = "openssl"))))]
        {
            let _ = (der, password);
            Err(Error::Unsupported(
                "PKCS#12 loading requires the openssl feature on Unix".into(),
            ))
        }
    }

    /// Loads an identity from a Windows system certificate store (Windows only).
    ///
    /// # Errors
    /// [`Error::NotFound`] if the store or a matching certificate is not found,
    /// [`Error::InvalidIdentity`] if it has no accessible private key,
    /// [`Error::Unsupported`] on non-Windows platforms.
    pub fn from_store(
        location: StoreLocation,
        store: &str,
        select: &StoreSelector,
    ) -> Result<Self, Error> {
        #[cfg(not(windows))]
        {
            let _ = (location, store, select);
            Err(Error::Unsupported(
                "certificate stores require Windows".into(),
            ))
        }
        #[cfg(windows)]
        {
            use crate::schannel::{CertStoreLocation, Certificate as SchannelCert};

            let location = match location {
                StoreLocation::CurrentUser => CertStoreLocation::CurrentUser,
                StoreLocation::LocalMachine => CertStoreLocation::LocalMachine,
            };
            let result = match select {
                StoreSelector::Thumbprint(tp) => SchannelCert::from_store(location, store, tp),
                StoreSelector::Subject(subject) => {
                    SchannelCert::from_store_by_subject(location, store, subject)
                }
            };
            let schannel = result.map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => Error::NotFound(e.to_string()),
                _ => Error::InvalidIdentity(e.to_string()),
            })?;
            Self::from_schannel(schannel)
        }
    }

    #[cfg(windows)]
    fn from_schannel(schannel: crate::schannel::Certificate) -> Result<Self, Error> {
        let (mut chain, key) = crate::x509::cng::from_schannel(schannel.clone())?;
        let cert = chain.remove(0);
        let key = PrivateKey::from_native(cert.clone(), key);
        Ok(Identity {
            cert,
            chain,
            key,
            schannel,
        })
    }

    /// Leaf certificate.
    #[must_use]
    pub fn cert(&self) -> &Certificate {
        &self.cert
    }

    /// Additional certificates, in input order or selected by the native chain engine.
    ///
    /// See [`Identity::ordered_chain`] for the chain to present.
    #[must_use]
    pub fn chain(&self) -> &[Certificate] {
        &self.chain
    }

    /// Chain to present after the leaf: the archive certificates ordered from the leaf's
    /// issuer upwards.
    ///
    /// The self-signed root is appended only if `include_root` is set. Issuers must match the
    /// certificate's issuer metadata and verify its signature; unrelated certificates are
    /// dropped with a warning and a self-signed leaf gets no chain.
    ///
    /// # Errors
    /// Certificate decoding errors or [`Error::Backend`] when the selected crypto provider
    /// is unavailable. Unsupported signatures cannot establish an issuer relationship.
    pub fn ordered_chain(&self, include_root: bool) -> Result<Vec<Certificate>, Error> {
        order_chain(&self.cert, self.chain.clone(), include_root)
    }

    /// Portable signing key; native keys need not be exportable.
    #[must_use]
    pub fn private_key(&self) -> &PrivateKey {
        &self.key
    }

    /// PKCS#8 DER private key (non-Windows compatibility accessor). Never log it.
    #[cfg(not(windows))]
    #[must_use]
    pub fn private_key_pkcs8_der(&self) -> &[u8] {
        &self.key.der
    }

    /// Schannel identity for `crate::schannel::ServerConfig::new` (Windows only).
    #[cfg(windows)]
    #[must_use]
    pub fn schannel(&self) -> &crate::schannel::Certificate {
        &self.schannel
    }
}

impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Identity")
            .field("cert", &self.cert)
            .field("chain", &self.chain())
            .field("key", &"<redacted>")
            .finish()
    }
}

/// Outbound TLS client identity, see the module docs.
#[derive(Clone)]
pub struct ClientIdentity {
    chain: Vec<Certificate>,
    key: PrivateKey,
}

impl ClientIdentity {
    /// Loads a leaf-first DER certificate list and an unencrypted PKCS#8 key.
    ///
    /// # Errors
    /// See [`Identity::from_der`]. The resulting chain omits the self-signed root.
    pub fn from_der(certificates: &[&[u8]], private_key: &[u8]) -> Result<Self, Error> {
        let certs = certificates
            .iter()
            .map(|der| Certificate::from_der(ntex_bytes::Bytes::copy_from_slice(der)))
            .collect::<Result<_, _>>()?;
        Self::from_certificates(certs, private_key)
    }

    /// Loads PEM certificates and an unencrypted PKCS#8 key, without persisting native keys.
    ///
    /// # Errors
    /// See [`Identity::from_pem`].
    pub fn from_pem(certificates: &[u8], private_key: &[u8]) -> Result<Self, Error> {
        let key = pkcs8_pem(private_key)?;
        Self::from_certificates(Certificate::from_pem_multi(certificates)?, &key)
    }

    fn from_certificates(mut certs: Vec<Certificate>, private_key: &[u8]) -> Result<Self, Error> {
        if certs.is_empty() {
            return Err(Error::InvalidIdentity("Certificate is missing".into()));
        }
        let cert = certs.remove(0);
        let key = PrivateKey::from_pkcs8(&cert, private_key)?;
        let mut chain = vec![cert.clone()];
        chain.extend(order_chain(&cert, certs, false)?);
        Ok(Self { chain, key })
    }

    /// Loads the client identity.
    ///
    /// # Errors
    /// See [`IdentitySource::load`]; on Windows also [`Error::Backend`] if the chain cannot be
    /// built and [`Error::Unsupported`] for key algorithms other than RSA and EC P-256/384/521.
    /// On Unix, [`Error::InvalidIdentity`] if an issuer certificate or public key cannot be decoded.
    pub fn load(source: &IdentitySource) -> Result<Self, Error> {
        #[cfg(not(windows))]
        {
            Self::from_identity(source.load()?)
        }
        #[cfg(windows)]
        {
            let (chain, key) = match source {
                IdentitySource::Pkcs12File(path) => {
                    let der =
                        std::fs::read(path).map_err(|e| Error::NotFound(format!("{path}: {e}")))?;
                    return Self::from_pkcs12(&der, "");
                }
                IdentitySource::PemFiles {
                    certificates,
                    private_key,
                } => {
                    let certs = std::fs::read(certificates)
                        .map_err(|e| Error::NotFound(format!("{certificates}: {e}")))?;
                    let key = zeroize::Zeroizing::new(
                        std::fs::read(private_key)
                            .map_err(|e| Error::NotFound(format!("{private_key}: {e}")))?,
                    );
                    return Self::from_pem(&certs, &key);
                }
                IdentitySource::Store {
                    location,
                    store,
                    select,
                } => crate::x509::cng::from_store(*location, store, select)?,
            };
            let key = PrivateKey::from_native(chain[0].clone(), key);
            Ok(ClientIdentity { chain, key })
        }
    }

    /// Parses a DER PKCS#12 archive; on Windows the key is imported into CNG without persisting.
    ///
    /// # Errors
    /// [`Error::InvalidDer`] if the input is not a PKCS#12 archive,
    /// [`Error::InvalidIdentity`] if the archive cannot be decrypted, has no key, or an issuer
    /// certificate or public key cannot be decoded (Unix),
    /// [`Error::Unsupported`] for unsupported key algorithms or unavailable PKCS#12 loading.
    pub fn from_pkcs12(der: &[u8], password: &str) -> Result<Self, Error> {
        #[cfg(not(windows))]
        {
            Identity::from_pkcs12(der, password).and_then(Self::from_identity)
        }
        #[cfg(windows)]
        {
            let (chain, key) = crate::x509::cng::from_pkcs12(der, password)?;
            let key = PrivateKey::from_native(chain[0].clone(), key);
            Ok(ClientIdentity { chain, key })
        }
    }

    /// Converts an identity into a leaf-first client chain without a self-signed root.
    ///
    /// # Errors
    /// See [`Identity::ordered_chain`].
    pub fn from_identity(id: Identity) -> Result<Self, Error> {
        let mut chain = vec![id.cert.clone()];
        chain.extend(order_chain(&id.cert, id.chain, false)?);
        Ok(ClientIdentity { chain, key: id.key })
    }

    /// Leaf certificate.
    #[must_use]
    pub fn cert(&self) -> &Certificate {
        &self.chain[0]
    }

    /// Leaf first, then intermediates in issuer order; the self-signed root is not included.
    #[must_use]
    pub fn chain(&self) -> &[Certificate] {
        &self.chain
    }

    /// Portable signing key; native keys need not be exportable.
    #[must_use]
    pub fn private_key(&self) -> &PrivateKey {
        &self.key
    }

    /// PKCS#8 DER private key (non-Windows compatibility accessor). Never log it.
    #[cfg(not(windows))]
    #[must_use]
    pub fn private_key_pkcs8_der(&self) -> &[u8] {
        &self.key.der
    }

    /// CNG private key (Windows only).
    #[cfg(windows)]
    #[must_use]
    pub fn key(&self) -> &CngKey {
        &self.key.native
    }
}

impl fmt::Debug for ClientIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientIdentity")
            .field("chain", &self.chain)
            .field("key", &"<redacted>")
            .finish()
    }
}

/// Orders the archive certificates into the presented chain: intermediates from the leaf's
/// issuer upwards, followed by the self-signed root if `include_root` is set.
///
/// Issuers must match the certificate's issuer metadata and verify its signature.
/// Unrelated certificates are dropped with a warning; a self-signed leaf gets no chain.
fn order_chain(
    leaf: &Certificate,
    extras: Vec<Certificate>,
    include_root: bool,
) -> Result<Vec<Certificate>, Error> {
    let mut current = leaf.clone();
    if crypto::is_self_signed(&current)? {
        if !extras.is_empty() {
            log::warn!("TLS certificate is self-signed, but certificate chain was provided");
        }
        return Ok(Vec::new());
    }
    let mut extras = extras;
    let mut chain = Vec::new();
    loop {
        let mut issuer = None;
        for (pos, candidate) in extras.iter().enumerate() {
            if crypto::is_issuer(&current, candidate)? {
                issuer = Some(pos);
                break;
            }
        }
        let Some(pos) = issuer else { break };
        let cert = extras.swap_remove(pos);
        if crypto::is_self_signed(&cert)? {
            if include_root {
                chain.push(cert);
            }
            break;
        }
        chain.push(cert.clone());
        current = cert;
    }
    if !extras.is_empty() {
        log::warn!("TLS certificate chain contains extra certificates");
    }
    Ok(chain)
}

/// Decodes exactly one unencrypted PKCS#8 PEM block without retaining plaintext copies.
pub(crate) fn pkcs8_pem(pem: &[u8]) -> Result<zeroize::Zeroizing<Vec<u8>>, Error> {
    let text = std::str::from_utf8(pem).map_err(|e| Error::InvalidPem(e.to_string()))?;
    for block in text.split("-----BEGIN ").skip(1) {
        if let Some((label, _)) = block.split_once("-----")
            && label.ends_with("PRIVATE KEY")
            && label != "PRIVATE KEY"
        {
            return Err(Error::InvalidPem(
                "only unencrypted PKCS#8 PRIVATE KEY blocks are supported".into(),
            ));
        }
    }
    let begin = "-----BEGIN PRIVATE KEY-----";
    let end = "-----END PRIVATE KEY-----";
    let mut starts = text.match_indices(begin);
    let (start, _) = starts.next().ok_or_else(|| {
        Error::InvalidPem("one unencrypted PKCS#8 PRIVATE KEY is required".into())
    })?;
    if starts.next().is_some() {
        return Err(Error::InvalidPem("multiple PRIVATE KEY blocks".into()));
    }
    let body = &text[start + begin.len()..];
    let end = body
        .find(end)
        .ok_or_else(|| Error::InvalidPem("missing PRIVATE KEY END".into()))?;
    let encoded = zeroize::Zeroizing::new(
        body[..end]
            .bytes()
            .filter(|b| !b.is_ascii_whitespace())
            .collect::<Vec<_>>(),
    );
    BASE64
        .decode(encoded.as_slice())
        .map(zeroize::Zeroizing::new)
        .map_err(|e| Error::InvalidPem(e.to_string()))
}

/// Converts a raw IEEE P1363 ECDSA signature (`r || s`, as returned by CNG) to the DER
/// `ECDSA-Sig-Value` used by TLS.
#[cfg(any(windows, all(test, unix, feature = "openssl")))]
pub(crate) fn ecdsa_p1363_to_der(raw: &[u8]) -> Vec<u8> {
    fn push_len(out: &mut Vec<u8>, len: usize) {
        // P-521 signatures need the long form, lengths stay below 256
        if len >= 0x80 {
            out.push(0x81);
        }
        out.push(u8::try_from(len).unwrap_or(u8::MAX));
    }

    let (r, s) = raw.split_at(raw.len() / 2);
    let mut body = Vec::with_capacity(raw.len() + 8);
    for int in [r, s] {
        let int = int
            .iter()
            .position(|b| *b != 0)
            .map_or(&[0u8][..], |i| &int[i..]);
        let pad = int[0] & 0x80 != 0;
        body.push(0x02);
        push_len(&mut body, int.len() + usize::from(pad));
        if pad {
            body.push(0);
        }
        body.extend_from_slice(int);
    }
    let mut out = Vec::with_capacity(body.len() + 3);
    out.push(0x30);
    push_len(&mut out, body.len());
    out.extend_from_slice(&body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::x509::testgen::{self, San};

    /// STATEMENT: PEM and DER identities load matching keys and produce usable TLS adapters.
    ///
    /// HOW:
    /// - Load the checked-in RSA identity through both constructors and the client facade.
    /// - Sign a message, reject an EC scheme, and verify via the available TLS backend.
    /// - Check exportable keys on non-Windows and non-exportable native keys on Windows.
    #[test]
    fn pem_der_identities() {
        crate::x509::test_provider();
        let pem = include_bytes!("../../examples/cert.pem");
        let key_pem = include_bytes!("../../examples/key.pem");
        let key_der = pkcs8_pem(key_pem).unwrap();
        let identity = Identity::from_pem(pem, key_pem).unwrap();
        let der_identity = Identity::from_der(&[identity.cert().der()], &key_der).unwrap();
        assert_eq!(identity.cert(), der_identity.cert());
        let client = ClientIdentity::from_pem(pem, key_pem).unwrap();
        let der_client = ClientIdentity::from_der(&[identity.cert().der()], &key_der).unwrap();
        assert_eq!(client.chain(), der_client.chain());
        assert_eq!(client.cert(), identity.cert());
        assert_eq!(
            client.private_key().public_key_der(),
            identity.cert().public_key_der()
        );
        assert!(format!("{:?}", client.private_key()).contains("<redacted>"));
        let key = client.private_key().clone();
        drop(client);
        let message = b"portable identity signing";
        let signature = key
            .sign(SignScheme::RsaPss(HashAlg::Sha256), message)
            .unwrap();
        assert!(matches!(
            key.sign(SignScheme::Ecdsa(HashAlg::Sha256), message),
            Err(Error::Unsupported(_))
        ));
        #[cfg(not(windows))]
        assert_eq!(key.export_pkcs8_der().unwrap(), &*key_der);
        #[cfg(windows)]
        assert!(matches!(key.export_pkcs8_der(), Err(Error::Unsupported(_))));
        #[cfg(feature = "rustls")]
        {
            use tls_rustls::pki_types::CertificateDer;
            let certified = crate::rustls::certified_key(&identity).unwrap();
            certified.keys_match().unwrap();
            let cert_der = CertificateDer::from(identity.cert().der().to_vec());
            let cert = webpki::EndEntityCert::try_from(&cert_der).unwrap();
            let provider = tls_rustls::crypto::ring::default_provider();
            let algorithms = provider
                .signature_verification_algorithms
                .mapping
                .iter()
                .find(|(scheme, _)| *scheme == tls_rustls::SignatureScheme::RSA_PSS_SHA256)
                .unwrap()
                .1;
            assert!(
                algorithms
                    .iter()
                    .any(|alg| cert.verify_signature(*alg, message, &signature).is_ok())
            );
            let signer = certified
                .key
                .choose_scheme(&[tls_rustls::SignatureScheme::RSA_PSS_SHA256])
                .unwrap();
            drop(certified);
            drop(identity);
            let signature = signer.sign(message).unwrap();
            assert!(
                algorithms
                    .iter()
                    .any(|alg| cert.verify_signature(*alg, message, &signature).is_ok())
            );
        }
        #[cfg(all(not(windows), not(feature = "rustls"), feature = "openssl"))]
        {
            use tls_openssl::{
                hash::MessageDigest,
                rsa::Padding,
                sign::{RsaPssSaltlen, Verifier},
                ssl,
            };
            let mut context = ssl::SslContextBuilder::new(ssl::SslMethod::tls()).unwrap();
            crate::openssl::set_identity(&mut context, &identity).unwrap();
            let public =
                tls_openssl::pkey::PKey::public_key_from_der(key.public_key_der()).unwrap();
            let mut verifier = Verifier::new(MessageDigest::sha256(), &public).unwrap();
            verifier.set_rsa_padding(Padding::PKCS1_PSS).unwrap();
            verifier.set_rsa_mgf1_md(MessageDigest::sha256()).unwrap();
            verifier
                .set_rsa_pss_saltlen(RsaPssSaltlen::DIGEST_LENGTH)
                .unwrap();
            assert!(verifier.verify_oneshot(&signature, message).unwrap());
        }
        #[cfg(all(windows, not(feature = "rustls")))]
        assert!(!signature.is_empty());
    }

    /// STATEMENT: Identity loading rejects mismatched, absent and malformed key material.
    ///
    /// HOW:
    /// - Pair an EC certificate with the RSA fixture key and reject both facades.
    /// - Reject empty certificate lists, corrupt DER, missing keys and multiple PEM keys.
    /// - Reject invalid UTF-8/base64, missing end markers and unsupported key labels.
    #[test]
    fn invalid_pem_der_identities() {
        crate::x509::test_provider();
        let pem = include_bytes!("../../examples/cert.pem");
        let key = include_bytes!("../../examples/key.pem");
        let wrong = include_bytes!("../../tests/testdata/leaf.pem");
        assert!(matches!(
            Identity::from_pem(wrong, key),
            Err(Error::InvalidIdentity(_))
        ));
        assert!(matches!(
            ClientIdentity::from_pem(wrong, key),
            Err(Error::InvalidIdentity(_))
        ));
        assert!(Identity::from_der(&[], &pkcs8_pem(key).unwrap()).is_err());
        assert!(ClientIdentity::from_der(&[], &pkcs8_pem(key).unwrap()).is_err());
        let cert = Certificate::from_pem(pem).unwrap();
        assert!(Identity::from_der(&[cert.der()], b"invalid").is_err());
        assert!(ClientIdentity::from_der(&[b"invalid"], &pkcs8_pem(key).unwrap()).is_err());
        assert!(Identity::from_pem(pem, b"").is_err());
        let repeated = [key.as_slice(), key.as_slice()].concat();
        assert!(matches!(
            Identity::from_pem(pem, &repeated),
            Err(Error::InvalidPem(_))
        ));
        let mixed = [key.as_slice(), b"-----BEGIN RSA PRIVATE KEY-----"].concat();
        assert!(matches!(
            Identity::from_pem(pem, &mixed),
            Err(Error::InvalidPem(_))
        ));
        for invalid in [
            b"\xff".as_slice(),
            b"-----BEGIN PRIVATE KEY-----\nAAAA",
            b"-----BEGIN PRIVATE KEY-----\n!\n-----END PRIVATE KEY-----",
            b"-----BEGIN ENCRYPTED PRIVATE KEY-----\nAAAA\n-----END ENCRYPTED PRIVATE KEY-----",
            b"-----BEGIN EC PRIVATE KEY-----\nAAAA\n-----END EC PRIVATE KEY-----",
        ] {
            assert!(matches!(
                Identity::from_pem(pem, invalid),
                Err(Error::InvalidPem(_))
            ));
            assert!(matches!(
                ClientIdentity::from_pem(pem, invalid),
                Err(Error::InvalidPem(_))
            ));
        }
    }

    /// STATEMENT: PEM file-loading errors identify the missing input for both identity facades.
    ///
    /// HOW:
    /// - Use an existing certificate file and a nonexistent child path as each input in turn.
    /// - Check both loaders return `NotFound` with the failing path, without creating files.
    #[test]
    fn pem_source_missing_files() {
        let certificate = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/cert.pem");
        let missing = format!("{certificate}/missing");
        for (certificates, private_key) in [
            (missing.as_str(), certificate),
            (certificate, missing.as_str()),
        ] {
            let source = IdentitySource::PemFiles {
                certificates: certificates.into(),
                private_key: private_key.into(),
            };
            for result in [
                source.load().map(|_| ()),
                ClientIdentity::load(&source).map(|_| ()),
            ] {
                assert!(
                    matches!(result, Err(Error::NotFound(message)) if message.contains(&missing))
                );
            }
        }
    }

    /// STATEMENT: Combined PEM documents and file sources load the same portable identity.
    ///
    /// HOW:
    /// - Load both facades from checked-in PEM files and a combined in-memory document.
    /// - Compare their certificates and check the client adapter retains the ordered chain.
    #[test]
    fn pem_sources() {
        crate::x509::test_provider();
        let source = IdentitySource::PemFiles {
            certificates: concat!(env!("CARGO_MANIFEST_DIR"), "/examples/cert.pem").into(),
            private_key: concat!(env!("CARGO_MANIFEST_DIR"), "/examples/key.pem").into(),
        };
        let identity = source.load().unwrap();
        let client = ClientIdentity::load(&source).unwrap();
        let combined = zeroize::Zeroizing::new(
            [
                include_bytes!("../../examples/cert.pem").as_slice(),
                include_bytes!("../../examples/key.pem").as_slice(),
            ]
            .concat(),
        );
        assert_eq!(
            Identity::from_pem(&combined, &combined).unwrap().cert(),
            identity.cert()
        );
        assert_eq!(
            ClientIdentity::from_pem(&combined, &combined)
                .unwrap()
                .cert(),
            client.cert()
        );
        #[cfg(feature = "rustls")]
        {
            let certified = crate::rustls::client_certified_key(&client);
            certified.keys_match().unwrap();
            assert_eq!(certified.cert[0].as_ref(), identity.cert().der().as_ref());
        }
    }

    /// STATEMENT: Shared chain ordering follows signatures and optionally includes the root.
    ///
    /// HOW:
    /// - Order fixed root/intermediate/leaf fixtures with an unrelated certificate first.
    /// - Check the root flag, a missing issuer and a corrupted intermediate signature.
    #[test]
    fn portable_chain_ordering() {
        crate::x509::test_provider();
        let leaf = Certificate::from_pem(include_bytes!("../../tests/testdata/leaf.pem")).unwrap();
        let inter =
            Certificate::from_pem(include_bytes!("../../tests/testdata/inter.pem")).unwrap();
        let root = Certificate::from_pem(include_bytes!("../../tests/testdata/root.pem")).unwrap();
        let other =
            Certificate::from_pem(include_bytes!("../../tests/testdata/other.pem")).unwrap();
        let extras = vec![other, root.clone(), inter.clone()];
        assert_eq!(
            order_chain(&leaf, extras.clone(), false).unwrap(),
            vec![inter.clone()]
        );
        assert_eq!(
            order_chain(&leaf, extras, true).unwrap(),
            vec![inter.clone(), root.clone()]
        );
        assert_eq!(
            order_chain(&leaf, vec![root.clone()], true).unwrap(),
            Vec::<Certificate>::new()
        );
        let mut der = inter.der().to_vec();
        *der.last_mut().unwrap() ^= 1;
        let corrupt = Certificate::from_der(der).unwrap();
        assert_eq!(
            order_chain(&corrupt, vec![root], true).unwrap(),
            Vec::<Certificate>::new()
        );
    }

    /// STATEMENT: PKCS#12 has no implicit OpenSSL dependency in rustls-only builds.
    ///
    /// HOW:
    /// - Call both PKCS#12 constructors without the OpenSSL feature.
    /// - Check capability errors are returned before attempting archive decoding.
    #[cfg(all(not(windows), not(feature = "openssl")))]
    #[test]
    fn pkcs12_requires_openssl() {
        assert!(matches!(
            Identity::from_pkcs12(b"archive", ""),
            Err(Error::Unsupported(_))
        ));
        assert!(matches!(
            ClientIdentity::from_pkcs12(b"archive", ""),
            Err(Error::Unsupported(_))
        ));
    }

    /// STATEMENT: Identity-source parsing preserves paths and rejects unavailable store sources.
    ///
    /// HOW:
    /// - Parse paths, thumbprint and subject selectors, and malformed store strings.
    /// - Check native selectors on Windows and unsupported errors elsewhere.
    #[test]
    fn source_from_str() {
        assert_eq!(
            "/etc/cert.pfx".parse::<IdentitySource>().unwrap(),
            IdentitySource::Pkcs12File("/etc/cert.pfx".into())
        );
        let store = "CERT:LocalMachine\\MY\\0102030405060708090a0b0c0d0e0f1011121314"
            .parse::<IdentitySource>();
        let subject = "cert:/CurrentUser/My/subject=gw.example.com".parse::<IdentitySource>();
        if cfg!(windows) {
            assert_eq!(
                store.unwrap(),
                IdentitySource::Store {
                    location: StoreLocation::LocalMachine,
                    store: "MY".into(),
                    select: StoreSelector::Thumbprint([
                        1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20
                    ]),
                }
            );
            assert_eq!(
                subject.unwrap(),
                IdentitySource::Store {
                    location: StoreLocation::CurrentUser,
                    store: "My".into(),
                    select: StoreSelector::Subject("gw.example.com".into()),
                }
            );
            for bad in [
                "cert:Other\\MY\\00",
                "cert:LocalMachine\\MY",
                "cert:LocalMachine\\MY\\abc",
                "cert:LocalMachine\\MY\\subject=",
            ] {
                assert!(
                    matches!(
                        bad.parse::<IdentitySource>(),
                        Err(Error::InvalidIdentity(_))
                    ),
                    "{bad}"
                );
            }
        } else {
            assert!(matches!(store, Err(Error::Unsupported(_))));
            assert!(matches!(subject, Err(Error::Unsupported(_))));
        }
    }

    /// STATEMENT: PKCS#12 imports preserve the matching key and extra certificates.
    ///
    /// HOW:
    /// - Generate and load a password-protected archive, comparing certificates and keys.
    /// - Reject a wrong password, malformed archive, and nonexistent source file.
    /// - On Windows the chain comes from the platform chain engine, which may not find the
    ///   untrusted root, so only its contents are checked.
    #[cfg(any(windows, all(unix, feature = "openssl")))]
    #[test]
    fn pkcs12_roundtrip() {
        let san = San::default();
        let rk = testgen::key();
        let root = testgen::cert("root", true, None, &rk, &san, (-1, 10));
        let lk = testgen::key();
        let leaf = testgen::cert("leaf", false, Some((&root, &rk)), &lk, &san, (-1, 10));
        let pfx = testgen::pkcs12(&leaf, &lk, &[&root], "pw");

        let id = Identity::from_pkcs12(&pfx, "pw").unwrap();
        assert_eq!(id.cert(), &leaf);
        if cfg!(windows) {
            assert!(id.chain().len() <= 1);
            assert!(id.chain().iter().all(|c| c == &root));
        } else {
            assert_eq!(id.chain(), [root]);
        }
        assert_eq!(id.private_key().public_key_der(), leaf.public_key_der());
        #[cfg(not(windows))]
        assert!(!format!("{id:?}").contains(&hex::encode(id.private_key_pkcs8_der())));
        assert!(format!("{id:?}").contains("<redacted>"));

        assert!(matches!(
            Identity::from_pkcs12(&pfx, "wrong"),
            Err(Error::InvalidIdentity(_))
        ));
        assert!(matches!(
            Identity::from_pkcs12(b"garbage", ""),
            Err(Error::InvalidDer(_))
        ));
        assert!(matches!(
            ClientIdentity::from_pkcs12(b"garbage", ""),
            Err(Error::InvalidDer(_))
        ));
        assert!(matches!(
            IdentitySource::Pkcs12File("/nonexistent/x.pfx".into()).load(),
            Err(Error::NotFound(_))
        ));
    }

    /// STATEMENT: Client identities present ordered intermediates without roots or unrelated certs;
    /// `Identity::ordered_chain` appends the root only on request.
    ///
    /// HOW:
    /// - Load unordered certificates and check the presented chain and private key.
    /// - Check `Identity::ordered_chain` with and without the root.
    /// - With OpenSSL, load the same chain from a PKCS#12 file and install it into a context.
    /// - Check self-signed leaves and missing intermediates yield no additional certificates.
    #[test]
    fn client_identity_chain() {
        let san = San::default();
        let rk = testgen::key();
        let root = testgen::cert("root", true, None, &rk, &san, (-1, 10));
        let ik = testgen::key();
        let inter = testgen::cert("inter", true, Some((&root, &rk)), &ik, &san, (-1, 10));
        let lk = testgen::key();
        let leaf = testgen::cert("leaf", false, Some((&inter, &ik)), &lk, &san, (-1, 10));
        let other = testgen::cert("other", true, None, &testgen::key(), &san, (-1, 10));

        // input order is unrelated to chain order, unrelated certificates are dropped
        let id = testgen::client(&leaf, &lk, &[&root, &other, &inter]);
        assert_eq!(id.chain(), [leaf.clone(), inter.clone()]);
        assert_eq!(id.cert(), &leaf);
        assert_eq!(id.cert().key_type(), Some(crate::x509::KeyType::EcP256));
        assert!(format!("{id:?}").contains("<redacted>"));

        let certs = [&leaf, &root, &other, &inter].map(|c| c.der().as_ref());
        let id = Identity::from_der(&certs, &lk.pkcs8_der()).unwrap();
        assert_eq!(
            id.ordered_chain(false).unwrap(),
            std::slice::from_ref(&inter)
        );
        assert_eq!(
            id.ordered_chain(true).unwrap(),
            [inter.clone(), root.clone()]
        );

        #[cfg(all(unix, feature = "openssl"))]
        {
            let pfx = testgen::pkcs12(&leaf, &lk, &[&root, &other, &inter], "");
            let path = std::env::temp_dir()
                .join(format!("ntex-tls-x509-client-{}.pfx", std::process::id()));
            std::fs::write(&path, &pfx).unwrap();
            let src = IdentitySource::Pkcs12File(path.to_string_lossy().into());
            let id = ClientIdentity::load(&src).unwrap();
            std::fs::remove_file(&path).unwrap();
            assert_eq!(id.chain(), [leaf.clone(), inter.clone()]);
            assert_eq!(id.private_key_pkcs8_der(), lk.pkcs8_der());

            let id = Identity::from_pkcs12(&pfx, "").unwrap();
            assert_eq!(
                id.ordered_chain(true).unwrap(),
                [inter.clone(), root.clone()]
            );
            let mut context =
                tls_openssl::ssl::SslContextBuilder::new(tls_openssl::ssl::SslMethod::tls())
                    .unwrap();
            crate::openssl::set_identity(&mut context, &id).unwrap();
            let context = context.build();
            let der = |c: &tls_openssl::x509::X509Ref| c.to_der().unwrap();
            assert_eq!(der(context.certificate().unwrap()), leaf.der().as_ref());
            assert_eq!(context.extra_chain_certs().len(), 1);
            assert_eq!(der(&context.extra_chain_certs()[0]), inter.der().as_ref());
        }

        // self-signed leaf: no chain
        assert_eq!(
            order_chain(&root, vec![inter.clone()], true).unwrap(),
            Vec::<Certificate>::new()
        );
        // missing intermediate: nothing to present
        let leaf2 = testgen::cert(
            "leaf2",
            false,
            Some((&inter, &ik)),
            &testgen::key(),
            &san,
            (-1, 10),
        );
        assert_eq!(
            order_chain(&leaf2, vec![root.clone()], true).unwrap(),
            Vec::<Certificate>::new()
        );
    }

    /// STATEMENT: OpenSSL issuer matching retains canonical distinguished-name comparison.
    ///
    /// HOW:
    /// - Create same-key roots whose names differ only by case and whitespace.
    /// - Order a leaf signed with one root using the other as its issuer.
    #[cfg(all(unix, feature = "openssl", not(feature = "rustls")))]
    #[test]
    fn openssl_canonical_issuer() {
        let san = San::default();
        let key = testgen::key();
        let root = testgen::cert("Root CA", true, None, &key, &san, (-1, 10));
        let alias = testgen::cert(" root  ca ", true, None, &key, &san, (-1, 10));
        let leaf = testgen::cert(
            "leaf",
            false,
            Some((&root, &key)),
            &testgen::key(),
            &san,
            (-1, 10),
        );
        assert_eq!(
            order_chain(&leaf, vec![alias.clone()], true).unwrap(),
            vec![alias]
        );
    }

    /// STATEMENT: Self-issued rollover certificates remain in the presented chain.
    ///
    /// HOW:
    /// - Generate a CA with its parent's subject but a new EC key, using EC and RSA parent keys.
    /// - Generate a leaf signed by the rollover CA.
    /// - Load identities with and without the old root and check the rollover CA is retained.
    /// - Load the rollover CA as the identity leaf and check its issuer is still followed.
    #[test]
    fn client_identity_rollover() {
        let san = San::default();
        for rk in [testgen::key(), testgen::rsa_key()] {
            let root = testgen::cert("root", true, None, &rk, &san, (-1, 10));
            let ik = testgen::key();
            let inter = testgen::cert("root", true, Some((&root, &rk)), &ik, &san, (-1, 10));
            let lk = testgen::key();
            let leaf = testgen::cert("leaf", false, Some((&inter, &ik)), &lk, &san, (-1, 10));
            for extras in [&[&root, &inter][..], &[&inter][..]] {
                let id = testgen::client(&leaf, &lk, extras);
                assert_eq!(id.chain(), [leaf.clone(), inter.clone()]);
            }

            // A self-issued leaf can also have a non-root issuer with the same subject.
            let next_key = testgen::key();
            let next = testgen::cert("root", true, Some((&inter, &ik)), &next_key, &san, (-1, 10));
            let id = testgen::client(&next, &next_key, &[&root, &inter]);
            assert_eq!(id.chain(), [next, inter]);
        }
    }

    /// STATEMENT: Same-subject issuer candidates are selected by key, not archive order.
    ///
    /// HOW:
    /// - Generate same-subject intermediates with different EC/RSA keys and no AKI extension.
    /// - Sign a leaf with one key and load both input orders.
    /// - Check only the intermediate whose key verifies the leaf is presented.
    #[test]
    fn client_identity_same_subject_issuers() {
        let san = San::default();
        let rk = testgen::key();
        let root = testgen::cert("root", true, None, &rk, &san, (-1, 10));
        let ik = testgen::key();
        let inter = testgen::cert("inter", true, Some((&root, &rk)), &ik, &san, (-1, 10));
        let lk = testgen::key();
        let leaf = testgen::cert("leaf", false, Some((&inter, &ik)), &lk, &san, (-1, 10));
        for key in [testgen::key(), testgen::rsa_key()] {
            let other = testgen::cert("inter", true, Some((&root, &rk)), &key, &san, (-1, 10));
            for extras in [[&other, &inter, &root], [&inter, &other, &root]] {
                let id = testgen::client(&leaf, &lk, &extras);
                assert_eq!(id.chain(), [leaf.clone(), inter.clone()]);
            }
        }
    }

    /// STATEMENT: CNG raw ECDSA signatures are encoded as canonical DER integers.
    ///
    /// HOW:
    /// - Compare 256/384/521-bit raw signatures with OpenSSL's DER encoder.
    /// - Include leading zeros and high-bit padding in both components.
    #[cfg(all(unix, feature = "openssl"))]
    #[test]
    fn ecdsa_der() {
        use tls_openssl::bn::BigNum;
        use tls_openssl::ecdsa::EcdsaSig;

        let check = |raw: Vec<u8>| {
            let (r, s) = raw.split_at(raw.len() / 2);
            let expected = EcdsaSig::from_private_components(
                BigNum::from_slice(r).unwrap(),
                BigNum::from_slice(s).unwrap(),
            )
            .unwrap()
            .to_der()
            .unwrap();
            assert_eq!(ecdsa_p1363_to_der(&raw), expected, "{}", hex::encode(&raw));
        };
        for len in [32, 48, 66] {
            check(vec![0x11; len * 2]);
            check(vec![0xff; len * 2]);
            let mut raw = vec![0u8; len * 2];
            raw[len - 1] = 1;
            raw[len] = 0x80;
            raw[2 * len - 1] = 7;
            check(raw);
        }
    }
}
