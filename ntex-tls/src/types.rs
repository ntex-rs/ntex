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
