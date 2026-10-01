use std::{any, cmp, io, io::IoSlice, io::Write, ops::Deref, ops::DerefMut, task::Poll};

use ntex_bytes::{BufMut, BytePage, BytePages};
use ntex_io::{FilterBuf, types};
use tls_rustls::{ConnectionCommon, SideData};

use super::{PeerCert, PeerCertChain};

pub(crate) struct Stream<'a, S> {
    pub(crate) session: &'a mut S,
}

impl<'a, S> Stream<'a, S> {
    pub(crate) fn new(session: &'a mut S) -> Self {
        Self { session }
    }
}

impl<S, SD> Stream<'_, S>
where
    S: DerefMut + Deref<Target = ConnectionCommon<SD>>,
    SD: SideData,
{
    pub(crate) fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>> {
        const H2: &[u8] = b"h2";

        if id == any::TypeId::of::<types::HttpProtocol>() {
            let h2 = self
                .session
                .alpn_protocol()
                .is_some_and(|protos| protos.windows(2).any(|w| w == H2));

            let proto = if h2 {
                types::HttpProtocol::Http2
            } else {
                types::HttpProtocol::Http1
            };
            Some(Box::new(proto))
        } else if id == any::TypeId::of::<PeerCert<'_>>() {
            let cert = self.session.peer_certificates()?.first()?;
            Some(Box::new(PeerCert(cert.to_owned())))
        } else if id == any::TypeId::of::<PeerCertChain<'_>>() {
            let chain = self.session.peer_certificates()?;
            Some(Box::new(PeerCertChain(chain.to_vec())))
        } else {
            None
        }
    }

    pub(crate) fn process_read_buf(&mut self, buf: &FilterBuf<'_>) -> io::Result<()> {
        let result = buf.with_read_buffers(|r_src, r_dst| {
            let Some(src) = r_src else {
                return Ok(());
            };
            loop {
                match self.session.read_tls(src) {
                    Ok(_) => {}
                    Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => break,
                    Err(err) => return Err(err),
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
                } else if state.peer_has_closed() {
                    // peer sent close_notify, start graceful shutdown
                    buf.io().close();
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

    pub(crate) fn process_write_buf(&mut self, buf: &FilterBuf<'_>) -> io::Result<()> {
        buf.with_write_buffers(|w_src, w_dst| write_buf(&mut **self.session, w_src, w_dst))
    }

    /// Write pending tls records to the output buffer
    fn write_tls_records(&mut self, buf: &FilterBuf<'_>) -> io::Result<()> {
        buf.with_write_buffers(|_, w_dst| {
            while self.session.wants_write() {
                self.session.write_tls(w_dst)?;
            }
            Ok(())
        })
    }

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

/// Encrypts the write buffer into tls records in the output buffer.
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

        let Some(page) = w_src.take() else {
            return Ok(());
        };
        if write_page(session, page, w_src)? == 0 {
            // nothing is consumed, the session buffer is full
            return Ok(());
        }
    }
}

/// Writes `page` together with the following page, if any.
///
/// Every write produces its own records, so a small page, e.g. response
/// headers in front of a body, would otherwise cost a record.
///
/// Returns the number of consumed bytes.
fn write_page<SD: SideData>(
    session: &mut ConnectionCommon<SD>,
    mut page: BytePage,
    src: &mut BytePages,
) -> io::Result<usize> {
    let Some(mut next) = src.take() else {
        let n = session.writer().write(&page)?;
        page.advance_to(n);
        src.prepend(page);
        return Ok(n);
    };

    let n = session
        .writer()
        .write_vectored(&[IoSlice::new(&page), IoSlice::new(&next)])?;
    let used = cmp::min(n, page.len());
    page.advance_to(used);
    next.advance_to(n - used);
    src.prepend(next);
    src.prepend(page);
    Ok(n)
}
