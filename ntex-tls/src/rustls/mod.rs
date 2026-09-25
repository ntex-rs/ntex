//! An implementation of SSL streams for ntex backed by OpenSSL
use std::io;

use ntex_io::Io;
use tls_rustls::pki_types::CertificateDer;

mod accept;
mod client;
mod connect;
mod server;
mod stream;

pub use self::accept::TlsAcceptor;
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

/// Waits for more handshake input.
///
/// The read that reports eof may also carry the peer's last handshake flight,
/// so the first eof lets the caller check the handshake state once more, and
/// only a second one is reported as a failure.
async fn wait_for_read<F>(io: &Io<F>, eof: &mut bool) -> io::Result<()> {
    if *eof {
        return Err(io::Error::new(io::ErrorKind::NotConnected, "disconnected"));
    }
    if io.read_notify().await?.is_none() {
        *eof = true;
    }
    Ok(())
}
