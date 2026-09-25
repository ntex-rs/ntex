//! An implementation of TLS streams for ntex backed by rustls
use std::{future::Future, io};

use ntex_io::Io;
use ntex_util::time::{Millis, timeout_checked};
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

/// Drive the handshake until the session stops handshaking.
///
/// `state` reports the session's `(wants_write, is_handshaking)` flags.
async fn handshake<F>(io: &Io<F>, state: impl Fn() -> (bool, bool)) -> io::Result<()> {
    let mut eof = false;
    loop {
        let (wants_write, handshaking) = state();
        if wants_write {
            io.flush(false).await?;
        }
        if !handshaking {
            return Ok(());
        }
        if eof {
            return Err(io::Error::new(io::ErrorKind::NotConnected, "disconnected"));
        }
        // The read that reports eof may also carry the peer's last handshake
        // flight, so the handshake state is checked once more before the eof
        // is treated as a failure.
        eof = io.read_notify().await?.is_none();
    }
}

/// Run handshake with timeout, zero timeout disables it
async fn with_timeout<R>(
    timeout: Millis,
    fut: impl Future<Output = io::Result<R>>,
) -> io::Result<R> {
    timeout_checked(timeout, fut).await.unwrap_or_else(|()| {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "TLS Handshake timeout",
        ))
    })
}
