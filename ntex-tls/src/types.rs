//! Types intended for use with [`ntex_io::Filter::query`].

/// A TLS PSK identity.
///
/// Used in conjunction with [`ntex_io::Filter::query`]:
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PskIdentity(pub Vec<u8>);

/// The TLS SNI server name (DNS).
///
/// Used in conjunction with [`ntex_io::Filter::query`]:
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Servername(pub String);

/// The peer's end-entity certificate in DER encoding.
///
/// Used in conjunction with [`ntex_io::Filter::query`], supported by all tls
/// filters.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PeerCertDer(pub Vec<u8>);

/// The certificates sent by the peer in DER encoding, the end-entity
/// certificate first.
///
/// Used in conjunction with [`ntex_io::Filter::query`], supported by all tls
/// filters.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PeerCertChainDer(pub Vec<Vec<u8>>);

/// The negotiated TLS protocol version.
///
/// Used in conjunction with [`ntex_io::Filter::query`], supported by all tls
/// filters.
#[non_exhaustive]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum TlsVersion {
    /// TLS 1.0
    Tls10,
    /// TLS 1.1
    Tls11,
    /// TLS 1.2
    Tls12,
    /// TLS 1.3
    Tls13,
}

/// The negotiated cipher suite.
///
/// Used in conjunction with [`ntex_io::Filter::query`], supported by the
/// rustls and schannel filters and by the openssl filter with OpenSSL 1.1.1
/// or newer. The schannel filter reports `name` for the suites Schannel
/// supports only.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct TlsCipher {
    /// IANA cipher suite id, e.g. `0x1301`
    pub id: u16,
    /// IANA cipher suite name, e.g. `TLS_AES_128_GCM_SHA256`
    pub name: Option<&'static str>,
}
