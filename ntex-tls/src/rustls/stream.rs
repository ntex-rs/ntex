use std::cell::RefCell;
use std::io::{self, Read, Write};
use std::{any, cmp, future::poll_fn, ops::Deref, ops::DerefMut, task::Poll, task::ready};

use ntex_bytes::{BufMut, BytePages, BytesMut};
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

    pub(crate) fn process_read_buf(&mut self, buf: &mut FilterBuf<'_>) -> io::Result<()> {
        // get processed buffer
        buf.with_buffers(|io, r_src, r_dst, _, _| {
            if let Some(src) = r_src {
                loop {
                    match self.session.read_tls(src) {
                        Ok(_) => {}
                        Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => {
                            break;
                        }
                        Err(err) => return Err(err),
                    }
                    let state = self
                        .session
                        .process_new_packets()
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

                    let new_b = state.plaintext_bytes_to_read();
                    if new_b > 0 {
                        if r_dst.is_none() {
                            *r_dst = Some(io.cfg().read_buf().get());
                        }
                        if let Some(dst) = r_dst {
                            dst.reserve(new_b);

                            let chunk: &mut [u8] =
                                unsafe { &mut *(&raw mut *dst.chunk_mut() as *mut [u8]) };
                            let v = self.session.reader().read(chunk)?;
                            unsafe { dst.advance_mut(v) };
                        }
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

    pub(crate) fn process_write_buf(&mut self, buf: &mut FilterBuf<'_>) -> io::Result<()> {
        buf.with_buffers(|_, r_src, _, w_src, w_dst| {
            loop {
                // write to tls stream
                while let Some(mut page) = w_src.take() {
                    page.advance_to(self.session.writer().write(&page)?);
                    if !w_src.prepend(page) {
                        // whole buffer is consumed, write next one
                        continue;
                    }
                    break;
                }

                // write tls records to output buffer
                if self.session.wants_write() {
                    let mut io = Wrapper { r_src, w_dst };
                    loop {
                        match self.session.write_tls(&mut io) {
                            Ok(n) => {
                                if n == 0 {
                                    break;
                                }
                            }
                            Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => {
                                break;
                            }
                            Err(err) => return Err(err),
                        }
                    }
                    continue;
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
        let handshaking = io.with_buf(|buf| {
            let mut session = session.borrow_mut();
            buf.with_buffers(|_, r_src, _, _, w_dst| {
                let mut io = Wrapper { r_src, w_dst };

                while session.wants_read() {
                    let has_data = io.r_src.as_ref().is_some_and(|b| !b.is_empty());

                    if has_data {
                        if session.read_tls(&mut io)? == 0 {
                            return Err(io::Error::new(
                                io::ErrorKind::NotConnected,
                                "disconnected",
                            ));
                        }
                        session.process_new_packets().map_err(|err| {
                            // In case we have an alert to send describing this error,
                            // try a last-gasp write -- but don't predate the primary
                            // error.
                            let _ = session.write_tls(&mut io);
                            io::Error::new(io::ErrorKind::InvalidData, err)
                        })?;
                    } else {
                        break;
                    }
                }

                while session.wants_write() {
                    session.write_tls(&mut io).map(|_| ())?;
                }
                Ok(session.is_handshaking())
            })
        })??;

        if handshaking {
            poll_fn(|cx| {
                Poll::Ready(if ready!(io.poll_read_notify(cx))?.is_none() {
                    Err(io::Error::new(io::ErrorKind::NotConnected, "disconnected"))
                } else {
                    Ok(())
                })
            })
            .await?;
        } else {
            return Ok(());
        }
    }
}

pub(crate) struct Wrapper<'a> {
    r_src: &'a mut Option<BytesMut>,
    w_dst: &'a mut BytePages,
}

impl io::Read for Wrapper<'_> {
    fn read(&mut self, dst: &mut [u8]) -> io::Result<usize> {
        if let Some(buf) = self.r_src {
            let len = cmp::min(buf.len(), dst.len());
            if len > 0 {
                dst[..len].copy_from_slice(&buf[..len]);
                buf.advance_to(len);
                return Ok(len);
            }
        }
        Err(io::Error::new(io::ErrorKind::WouldBlock, ""))
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
