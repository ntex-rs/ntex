//! Buffer and shutdown operations shared by client and server rustls sessions.
use std::{any, io, io::IoSlice, io::Write, ops::Deref, ops::DerefMut, task::Poll};

use ntex_bytes::{BufMut, BytePages, Bytes};
use ntex_io::{FilterBuf, types};
use tls_rustls::{CipherSuite, ConnectionCommon, ProtocolVersion, SideData};

use super::{PeerCert, PeerCertChain};
use crate::{PeerCertChainDer, PeerCertDer, TlsCipher, TlsVersion};

/// Temporary adapter over an exclusively borrowed rustls connection.
///
/// The owning filter must release this adapter before sending notifications
/// that can synchronously reenter the filter.
pub(crate) struct Stream<'a, S> {
    /// Session borrowed for the duration of one filter operation.
    pub(crate) session: &'a mut S,
}

impl<'a, S> Stream<'a, S> {
    /// Wraps an existing session without allocating or transferring ownership.
    pub(crate) fn new(session: &'a mut S) -> Self {
        Self { session }
    }
}

impl<S, SD> Stream<'_, S>
where
    S: DerefMut + Deref<Target = ConnectionCommon<SD>>,
    SD: SideData,
{
    /// Produces owned metadata for both roles; server-only SNI is handled by the server filter.
    pub(crate) fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>> {
        if id == any::TypeId::of::<types::HttpProtocol>() {
            let alpn = self.session.alpn_protocol();
            Some(Box::new(crate::utils::http_protocol(alpn)))
        } else if id == any::TypeId::of::<PeerCert<'_>>() {
            let cert = self.session.peer_certificates()?.first()?;
            Some(Box::new(PeerCert(cert.to_owned())))
        } else if id == any::TypeId::of::<PeerCertChain<'_>>() {
            let chain = self.session.peer_certificates()?;
            Some(Box::new(PeerCertChain(chain.to_vec())))
        } else if id == any::TypeId::of::<PeerCertDer>() {
            let cert = self.session.peer_certificates()?.first()?;
            Some(Box::new(PeerCertDer(Bytes::copy_from_slice(cert))))
        } else if id == any::TypeId::of::<PeerCertChainDer>() {
            let chain = self.session.peer_certificates().filter(|c| !c.is_empty())?;
            Some(Box::new(PeerCertChainDer(
                chain
                    .iter()
                    .map(|cert| Bytes::copy_from_slice(cert))
                    .collect(),
            )))
        } else if id == any::TypeId::of::<TlsVersion>() {
            let version = match self.session.protocol_version()? {
                ProtocolVersion::TLSv1_0 => TlsVersion::Tls10,
                ProtocolVersion::TLSv1_1 => TlsVersion::Tls11,
                ProtocolVersion::TLSv1_2 => TlsVersion::Tls12,
                ProtocolVersion::TLSv1_3 => TlsVersion::Tls13,
                _ => return None,
            };
            Some(Box::new(version))
        } else if id == any::TypeId::of::<TlsCipher>() {
            let suite = self.session.negotiated_cipher_suite()?.suite();
            Some(Box::new(TlsCipher {
                id: u16::from(suite),
                name: cipher_name(suite),
            }))
        } else {
            None
        }
    }

    /// Processes available records and drains their plaintext without treating an empty buffer as EOF.
    ///
    /// Sets `peer_closed` instead of closing I/O while the session is borrowed.
    /// Post-handshake output, including key-update responses, is queued even
    /// when processing fails; the original processing error takes precedence.
    pub(crate) fn process_read_buf(
        &mut self,
        buf: &FilterBuf<'_>,
        peer_closed: &mut bool,
    ) -> io::Result<()> {
        let result = buf.with_read_buffers(|r_src, r_dst| {
            let Some(src) = r_src else {
                return Ok(());
            };
            loop {
                // rustls treats an empty read as transport EOF
                if !src.is_empty() {
                    self.session.read_tls(src)?;
                }
                let state = self
                    .session
                    .process_new_packets()
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

                let new_b = state.plaintext_bytes_to_read();
                if new_b > 0 {
                    r_dst.reserve(new_b);

                    let chunk: &mut [u8] =
                        unsafe { &mut *(&raw mut *r_dst.chunk_mut() as *mut [u8]) };
                    let v = io::Read::read(&mut self.session.reader(), chunk)?;
                    unsafe { r_dst.advance_mut(v) };
                }
                // every complete record is processed, so plaintext is drained
                if state.peer_has_closed() {
                    *peer_closed = true;
                    break;
                } else if src.is_empty() {
                    break;
                }
            }
            Ok::<_, io::Error>(())
        });

        // flush tls records generated while processing incoming data
        // (e.g. KeyUpdate responses), original error takes priority;
        // during handshake flushing is driven by the handshake helper
        if !self.session.is_handshaking() {
            // materialize tls records queued by the session, rustls keeps
            // KeyUpdate responses outside of its sendable tls buffer until
            // the next plaintext write; an empty write is a no-op otherwise
            let _ = self.session.writer().write(&[]);

            if self.session.wants_write() {
                let flushed = self.write_tls_records(buf);
                result?;
                return flushed;
            }
        }
        result
    }

    /// Encrypts queued plaintext while preserving bytes not yet accepted by the session.
    pub(crate) fn process_write_buf(&mut self, buf: &FilterBuf<'_>) -> io::Result<()> {
        buf.with_write_buffers(|w_src, w_dst| write_buf(&mut **self.session, w_src, w_dst))
    }

    /// Drains pending TLS records into the lower filter's output buffer.
    ///
    /// This queues ciphertext; it does not wait for transport delivery.
    fn write_tls_records(&mut self, buf: &FilterBuf<'_>) -> io::Result<()> {
        buf.with_write_buffers(|_, w_dst| {
            while self.session.wants_write() {
                self.session.write_tls(w_dst)?;
            }
            Ok(())
        })
    }

    /// Queues pending application data before `close_notify`, then waits for peer closure.
    ///
    /// Repeated calls are supported. Transport EOF ends the wait even if the
    /// peer never sends its own `close_notify`.
    pub(crate) fn shutdown(&mut self, buf: &FilterBuf<'_>) -> io::Result<Poll<()>> {
        // drain pending application data into tls records first
        self.process_write_buf(buf)?;

        // queue close_notify alert (idempotent, may be called on every poll)
        self.session.send_close_notify();

        // write pending tls records (incl. close_notify) to the output buffer
        self.write_tls_records(buf)?;

        if self.session.wants_write() {
            return Ok(Poll::Pending);
        }

        // wait for the peer's close_notify, unless the peer already closed
        // the connection and it is never going to arrive
        let state = self
            .session
            .process_new_packets()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        if state.peer_has_closed() || buf.io().is_read_eof() {
            Ok(Poll::Ready(()))
        } else {
            Ok(Poll::Pending)
        }
    }
}

/// Encrypts plaintext pages and drains generated TLS records into the output buffer.
///
/// Pending records are drained before each plaintext write because rustls counts
/// them against its buffer limit. Zero progress leaves unaccepted input queued
/// for a later call, including during handshakes or with a zero buffer limit.
pub(super) fn write_buf<SD: SideData>(
    session: &mut ConnectionCommon<SD>,
    w_src: &mut BytePages,
    w_dst: &mut BytePages,
) -> io::Result<()> {
    loop {
        // the session's buffer limit includes pending tls records, they are
        // moved to the output buffer first so a write can fill a record
        while session.wants_write() {
            match session.write_tls(w_dst) {
                Ok(0) => break,
                Ok(_) => {}
                Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) => return Err(err),
            }
        }

        if write_pages(session, w_src)? == 0 {
            // no input remains, or the session buffer is full
            return Ok(());
        }
    }
}

/// Writes up to 32 queued pages together without a plaintext gathering copy.
///
/// Every write produces its own records, so a small page, e.g. response
/// headers in front of a body, would otherwise cost a record.
///
/// Returns the number of consumed bytes and advances `src` by exactly that
/// amount. A partial write leaves the remaining page data available for retry.
fn write_pages<SD: SideData>(
    session: &mut ConnectionCommon<SD>,
    src: &mut BytePages,
) -> io::Result<usize> {
    let n = if src.num_pages() <= 1 {
        let Some(page) = src.iter().next() else {
            return Ok(0);
        };
        session.writer().write(page)?
    } else {
        let mut bufs = [IoSlice::new(&[]); 32];
        let mut num = 0;
        for (buf, page) in bufs.iter_mut().zip(src.iter()) {
            *buf = IoSlice::new(page);
            num += 1;
        }
        session.writer().write_vectored(&bufs[..num])?
    };

    src.advance_to(n);
    Ok(n)
}

/// Returns the IANA name, normalizing rustls's `TLS13_` names to `TLS_`.
fn cipher_name(suite: CipherSuite) -> Option<&'static str> {
    match suite {
        CipherSuite::TLS13_AES_128_GCM_SHA256 => Some("TLS_AES_128_GCM_SHA256"),
        CipherSuite::TLS13_AES_256_GCM_SHA384 => Some("TLS_AES_256_GCM_SHA384"),
        CipherSuite::TLS13_CHACHA20_POLY1305_SHA256 => Some("TLS_CHACHA20_POLY1305_SHA256"),
        CipherSuite::TLS13_AES_128_CCM_SHA256 => Some("TLS_AES_128_CCM_SHA256"),
        CipherSuite::TLS13_AES_128_CCM_8_SHA256 => Some("TLS_AES_128_CCM_8_SHA256"),
        _ => suite.as_str(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// STATEMENT: Cipher names normalize TLS 1.3 spelling, preserve TLS 1.2 names,
    /// and return no name for unknown suites.
    ///
    /// HOW:
    /// - Compare `cipher_name` against the exact IANA names for all five TLS 1.3 suites.
    /// - Check a TLS 1.2 suite retains its name and the unknown suite `0xffff` returns `None`.
    #[test]
    fn cipher_names() {
        for (suite, expected) in [
            (
                CipherSuite::TLS13_AES_128_GCM_SHA256,
                Some("TLS_AES_128_GCM_SHA256"),
            ),
            (
                CipherSuite::TLS13_AES_256_GCM_SHA384,
                Some("TLS_AES_256_GCM_SHA384"),
            ),
            (
                CipherSuite::TLS13_CHACHA20_POLY1305_SHA256,
                Some("TLS_CHACHA20_POLY1305_SHA256"),
            ),
            (
                CipherSuite::TLS13_AES_128_CCM_SHA256,
                Some("TLS_AES_128_CCM_SHA256"),
            ),
            (
                CipherSuite::TLS13_AES_128_CCM_8_SHA256,
                Some("TLS_AES_128_CCM_8_SHA256"),
            ),
            (
                CipherSuite::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
                Some("TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256"),
            ),
            (CipherSuite::from(0xffff), None),
        ] {
            assert_eq!(cipher_name(suite), expected, "{suite:?}");
        }
    }
}
