use std::{any, cell::RefCell, io, io::Write, ops::Deref, ops::DerefMut};

use ntex_bytes::{BufMut, BytePages};
use ntex_io::{FilterBuf, Io, types};
use tls_rustls::{ConnectionCommon, SideData};

use super::{PeerCert, PeerCertChain};

pub(crate) struct Stream<'a, S> {
    session: &'a mut S,
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
            if let Some(cert_chain) = self.session.peer_certificates() {
                if let Some(cert) = cert_chain.first() {
                    Some(Box::new(PeerCert(cert.to_owned())))
                } else {
                    None
                }
            } else {
                None
            }
        } else if id == any::TypeId::of::<PeerCertChain<'_>>() {
            if let Some(cert_chain) = self.session.peer_certificates() {
                Some(Box::new(PeerCertChain(cert_chain.to_vec())))
            } else {
                None
            }
        } else {
            None
        }
    }

    pub(crate) fn process_read_buf(&mut self, buf: &FilterBuf<'_>) -> io::Result<()> {
        buf.with_read_buffers(|r_src, r_dst| {
            if let Some(src) = r_src {
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
                    } else if src.is_empty() {
                        break;
                    }
                }
                Ok::<_, io::Error>(())
            } else {
                Ok(())
            }
        })
    }

    pub(crate) fn process_write_buf(&mut self, buf: &FilterBuf<'_>) -> io::Result<()> {
        buf.with_write_buffers(|w_src, w_dst| {
            'outer: loop {
                // write to tls stream
                while let Some(mut page) = w_src.take() {
                    page.advance_to(self.session.writer().write(&page)?);
                    if w_src.prepend(page) {
                        // buffer partially consumed, need to write_tls
                        break;
                    }
                }

                // write tls records to output buffer
                if self.session.wants_write() {
                    let mut wrp = Wrapper { w_dst, buf };
                    loop {
                        match self.session.write_tls(&mut wrp) {
                            Ok(0) => continue 'outer,
                            Ok(_) => {}
                            Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => {
                                continue 'outer;
                            }
                            Err(err) => return Err(err),
                        }
                    }
                }
                return Ok(());
            }
        })
    }
}

pub(crate) async fn handshake<F, S, SD>(
    session: &RefCell<S>,
    io: &Io<F>,
) -> Result<(), io::Error>
where
    S: DerefMut + Deref<Target = ConnectionCommon<SD>>,
    SD: SideData,
{
    loop {
        let (wants_write, handshaking) = {
            let s = session.borrow_mut();
            (s.wants_write(), s.is_handshaking())
        };

        if wants_write {
            io.flush(false).await?;
        }

        if handshaking {
            io.read_notify().await?.ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotConnected, "disconnected")
            })?;
        } else {
            return Ok(());
        }
    }
}

pub(crate) struct Wrapper<'a> {
    w_dst: &'a mut BytePages,
    buf: &'a FilterBuf<'a>,
}

impl io::Read for Wrapper<'_> {
    fn read(&mut self, dst: &mut [u8]) -> io::Result<usize> {
        self.buf.with_read_src(|r_src| {
            if let Some(b) = r_src {
                io::Read::read(b, dst)
            } else {
                Err(io::Error::new(io::ErrorKind::WouldBlock, ""))
            }
        })
    }
}

impl io::Write for Wrapper<'_> {
    fn write(&mut self, src: &[u8]) -> io::Result<usize> {
        self.w_dst.put_slice(src);
        Ok(src.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
