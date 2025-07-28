//! An implementation of SSL streams for ntex backed by OpenSSL
use std::io::{self, Read as IoRead, Write as IoWrite};
use std::{any, cell::RefCell, future::poll_fn, sync::Arc, task::ready, task::Poll};

use ntex_bytes::BufMut;
use ntex_io::{types, Filter, FilterLayer, Io, Layer, ReadBuf, WriteBuf};
use tls_rust::{pki_types::ServerName, ClientConfig, ClientConnection};

use super::{PeerCert, PeerCertChain, Wrapper};

#[derive(Debug)]
/// An implementation of SSL streams
pub struct TlsClientFilter {
    session: RefCell<ClientConnection>,
}

impl FilterLayer for TlsClientFilter {
    fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>> {
        const H2: &[u8] = b"h2";

        if id == any::TypeId::of::<types::HttpProtocol>() {
            let h2 = self
                .session
                .borrow()
                .alpn_protocol()
                .map(|protos| protos.windows(2).any(|w| w == H2))
                .unwrap_or(false);

            let proto = if h2 {
                types::HttpProtocol::Http2
            } else {
                types::HttpProtocol::Http1
            };
            Some(Box::new(proto))
        } else if id == any::TypeId::of::<PeerCert<'_>>() {
            if let Some(cert_chain) = self.session.borrow().peer_certificates() {
                if let Some(cert) = cert_chain.first() {
                    Some(Box::new(PeerCert(cert.to_owned())))
                } else {
                    None
                }
            } else {
                None
            }
        } else if id == any::TypeId::of::<PeerCertChain<'_>>() {
            if let Some(cert_chain) = self.session.borrow().peer_certificates() {
                Some(Box::new(PeerCertChain(cert_chain.to_vec())))
            } else {
                None
            }
        } else {
            None
        }
    }

    fn process_read_buf(&self, buf: &ReadBuf<'_>) -> io::Result<usize> {
        let mut session = self.session.borrow_mut();
        let mut new_bytes = 0;

        // get processed buffer
        buf.with_src(|src| {
            if let Some(src) = src {
                buf.with_dst(|dst| {
                    loop {
                        let mut cursor = io::Cursor::new(&src);
                        let n = match session.read_tls(&mut cursor) {
                            Ok(n) => n,
                            Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => {
                                break
                            }
                            Err(err) => return Err(err),
                        };
                        src.split_to(n);
                        let state = session
                            .process_new_packets()
                            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

                        let new_b = state.plaintext_bytes_to_read();
                        if new_b > 0 {
                            dst.reserve(new_b);
                            let chunk: &mut [u8] =
                                unsafe { std::mem::transmute(&mut *dst.chunk_mut()) };
                            let v = session.reader().read(chunk)?;
                            unsafe { dst.advance_mut(v) };
                            new_bytes += v;
                        } else {
                            break;
                        }
                    }
                    Ok::<_, io::Error>(())
                })?;
            }
            Ok(new_bytes)
        })
    }

    fn process_write_buf(&self, buf: &WriteBuf<'_>) -> io::Result<()> {
        buf.with_src(|src| {
            if let Some(src) = src {
                let mut io = Wrapper(buf);
                let mut session = self.session.borrow_mut();

                'outer: loop {
                    if !src.is_empty() {
                        src.split_to(session.writer().write(src)?);

                        loop {
                            match session.write_tls(&mut io) {
                                Ok(0) => continue 'outer,
                                Ok(_) => continue,
                                Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => {
                                    break
                                }
                                Err(err) => return Err(err),
                            }
                        }
                    }
                    break;
                }
            }
            Ok(())
        })
    }
}

impl TlsClientFilter {
    pub async fn create<F: Filter>(
        io: Io<F>,
        cfg: Arc<ClientConfig>,
        domain: ServerName<'static>,
    ) -> Result<Io<Layer<TlsClientFilter, F>>, io::Error> {
        let session = ClientConnection::new(cfg, domain).map_err(io::Error::other)?;
        let filter = TlsClientFilter {
            session: RefCell::new(session),
        };
        let io = io.add_filter(filter);

        let filter = io.filter();
        loop {
            let (result, handshaking) = io.with_buf(|buf| {
                let mut wrp = Wrapper(buf);
                let mut session = filter.session.borrow_mut();
                let mut result = Err(io::Error::new(io::ErrorKind::WouldBlock, ""));

                while session.wants_write() {
                    result = session.write_tls(&mut wrp).map(|_| ());
                    if result.is_err() {
                        break
                    }
                }
                if session.wants_read() {
                    let has_data = buf.with_read_buf(|rbuf| {
                        rbuf.with_src(|b| {
                            b.as_ref().map(|b| !b.is_empty()).unwrap_or_default()
                        })
                    });

                    if has_data {
                        result = match session.read_tls(&mut wrp) {
                            Ok(0) => Err(io::Error::new(
                                io::ErrorKind::NotConnected,
                                "disconnected",
                            )),
                            Ok(_) => Ok(()),
                            Err(e) => Err(e),
                        };

                        session.process_new_packets().map_err(|err| {
                            // In case we have an alert to send describing this error,
                            // try a last-gasp write -- but don't predate the primary
                            // error.
                            let _ = session.write_tls(&mut wrp);
                            io::Error::new(io::ErrorKind::InvalidData, err)
                        })?;
                    }
                }

                Ok::<_, io::Error>((result, session.is_handshaking()))
            })??;

            match result {
                Ok(()) => return Ok(io),
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if !handshaking {
                        return Ok(io);
                    }
                    poll_fn(|cx| {
                        match ready!(io.poll_read_notify(cx))? {
                            Some(_) => Ok(()),
                            None => Err(io::Error::new(
                                io::ErrorKind::NotConnected,
                                "disconnected",
                            )),
                        }?;
                        Poll::Ready(Ok::<_, io::Error>(()))
                    })
                    .await?;
                }
                Err(e) => return Err(e),
            }
        }
    }
}
