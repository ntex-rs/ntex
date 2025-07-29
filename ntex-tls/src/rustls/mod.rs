//! An implementation of SSL streams for ntex backed by OpenSSL
use tls_rust::pki_types::CertificateDer;

mod accept;
mod client;
mod connect;
mod server;
mod stream;

pub use self::accept::{TlsAcceptor, TlsAcceptorService};
pub use self::client::TlsClientFilter;
pub use self::connect::TlsConnector;
pub use self::server::TlsServerFilter;

use self::stream::Stream;

/// Connection's peer cert
#[derive(Debug)]
pub struct PeerCert<'a>(pub CertificateDer<'a>);

/// Connection's peer cert chain
#[derive(Debug)]
pub struct PeerCertChain<'a>(pub Vec<CertificateDer<'a>>);
