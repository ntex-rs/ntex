//! An implementation of SSL streams for ntex backed by OpenSSL
use std::{cmp, io};

use ntex_io::WriteBuf;
use tls_rust::Certificate;

mod accept;
mod client;
mod server;
pub use accept::{TlsAcceptor, TlsAcceptorService};

pub use self::client::TlsClientFilter;
pub use self::server::TlsServerFilter;

/// Connection's peer cert
#[derive(Debug)]
pub struct PeerCert(pub Certificate);

/// Connection's peer cert chain
#[derive(Debug)]
pub struct PeerCertChain(pub Vec<Certificate>);

pub(crate) struct Wrapper<'a, 'b>(&'a WriteBuf<'b>);

impl<'a, 'b> io::Read for Wrapper<'a, 'b> {
    fn read(&mut self, dst: &mut [u8]) -> io::Result<usize> {
        self.0.with_read_buf(|buf| {
            buf.with_src(|buf| {
                if let Some(buf) = buf {
                    let len = cmp::min(buf.len(), dst.len());
                    if len > 0 {
                        dst[..len].copy_from_slice(&buf.split_to(len));
                        return Ok(len);
                    }
                }
                Err(io::Error::new(io::ErrorKind::WouldBlock, ""))
            })
        })
    }
}

impl<'a, 'b> io::Write for Wrapper<'a, 'b> {
    fn write(&mut self, src: &[u8]) -> io::Result<usize> {
        self.0.with_dst(|buf| buf.extend_from_slice(src));
        Ok(src.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
