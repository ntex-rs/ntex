//! Owned connection metadata returned through [`ntex_io::IoRef::query`].
//!
//! Queries return no value when a backend does not support the requested type
//! or the negotiated session has no corresponding information. Reading a
//! certificate is not a substitute for configuring certificate verification.

use ntex_bytes::Bytes;

/// The pre-shared-key identity reported by OpenSSL.
///
/// Available only when OpenSSL exposes an identity for the session. The rustls
/// and Schannel filters do not implement this query.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PskIdentity(
    /// Identity bytes, not the secret key.
    pub Bytes,
);

/// The DNS server name carried in the TLS SNI extension.
///
/// Server-side filters expose the name received from the client, when present.
/// OpenSSL may also expose its configured client-side name. This is not the
/// transport address or evidence that the peer is authorized to use the name.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Servername(
    /// Owned server name without a port.
    pub String,
);

/// The peer's end-entity certificate in DER encoding.
///
/// Supported by all TLS backends. The query returns no value if no peer
/// certificate is available, for example when client authentication is not used.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PeerCertDer(
    /// DER-encoded leaf certificate.
    pub Bytes,
);

/// The certificates sent by the peer in DER encoding, the end-entity
/// certificate first.
///
/// Supported by all TLS backends. OpenSSL normalizes its native chain to include
/// the leaf on both sides; Schannel orders issuers after the leaf and retains
/// unrelated supplied certificates at the end. The chain need not contain a
/// root certificate and its presence does not establish trust.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PeerCertChainDer(
    /// DER-encoded certificates, with the peer's leaf first.
    pub Vec<Bytes>,
);

/// The negotiated TLS protocol version.
///
/// Supported by all TLS backends. These variants describe metadata, not the
/// protocol versions enabled by a backend's configuration.
#[non_exhaustive]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum TlsVersion {
    /// TLS 1.0.
    Tls10,
    /// TLS 1.1.
    Tls11,
    /// TLS 1.2.
    Tls12,
    /// TLS 1.3.
    Tls13,
}

/// The negotiated cipher suite.
///
/// Supported by rustls, Schannel, and OpenSSL builds with cipher metadata
/// support (OpenSSL 1.1.1 or newer). A known numeric identifier may have no
/// corresponding name in the backend's lookup table.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct TlsCipher {
    /// IANA cipher suite identifier, for example `0x1301`.
    pub id: u16,
    /// IANA name, for example `TLS_AES_128_GCM_SHA256`, when recognized.
    pub name: Option<&'static str>,
}
