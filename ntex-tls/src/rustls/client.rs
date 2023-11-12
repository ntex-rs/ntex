//! An implementation of SSL streams for ntex backed by OpenSSL
use std::io::{self, Read as IoRead, Write as IoWrite};
use std::{any, cell::RefCell, sync::Arc, task::Poll};

use ntex_bytes::BufMut;
use ntex_io::{types, Filter, FilterLayer, Io, Layer, ReadBuf, WriteBuf};
use ntex_util::{future::poll_fn, ready};
use tls_rust::{ClientConfig, ClientConnection, ServerName};

use crate::rustls::{TlsFilter, Wrapper};

use super::{PeerCert, PeerCertChain};

#[derive(Debug)]
/// An implementation of SSL streams
pub(crate) struct TlsClientFilter {
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
        } else if id == any::TypeId::of::<PeerCert>() {
            if let Some(cert_chain) = self.session.borrow().peer_certificates() {
                if let Some(cert) = cert_chain.first() {
                    Some(Box::new(PeerCert(cert.to_owned())))
                } else {
                    None
                }
            } else {
                None
            }
        } else if id == any::TypeId::of::<PeerCertChain>() {
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
                        let n = session.read_tls(&mut cursor)?;
                        src.split_to(n);
                        let state = session
                            .process_new_packets()
                            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

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
                let mut session = self.session.borrow_mut();
                let mut io = Wrapper(buf);

                loop {
                    if !src.is_empty() {
                        src.split_to(session.writer().write(src)?);
                    }
                    if session.wants_write() {
                        session.complete_io(&mut io)?;
                    } else {
                        break;
                    }
                }
            }
            Ok(())
        })
    }
}

impl TlsClientFilter {
    pub(crate) async fn create<F: Filter>(
        io: Io<F>,
        cfg: Arc<ClientConfig>,
        domain: ServerName,
    ) -> Result<Io<Layer<TlsFilter, F>>, io::Error> {
        let session = ClientConnection::new(cfg, domain)
            .map_err(|err| io::Error::new(io::ErrorKind::Other, err))?;
        let filter = TlsFilter::new_client(TlsClientFilter {
            session: RefCell::new(session),
        });
        let io = io.add_filter(filter);

        let filter = io.filter();
        loop {
            let (result, wants_read, handshaking) = io.with_buf(|buf| {
                let mut session = filter.client().session.borrow_mut();
                let mut wrp = Wrapper(buf);
                let mut result = (
                    session.complete_io(&mut wrp),
                    session.wants_read(),
                    session.is_handshaking(),
                );

                if result.0.is_ok() && session.wants_write() {
                    result.0 = session.complete_io(&mut wrp);
                }
                result
            })?;

            match result {
                Ok(_) => {
                    return Ok(io);
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if !handshaking {
                        return Ok(io);
                    }
                    poll_fn(|cx| {
                        let read_ready = if wants_read {
                            match ready!(io.poll_force_read_ready(cx))? {
                                Some(_) => Ok(true),
                                None => Err(io::Error::new(
                                    io::ErrorKind::Other,
                                    "disconnected",
                                )),
                            }?
                        } else {
                            true
                        };
                        if read_ready {
                            Poll::Ready(Ok::<_, io::Error>(()))
                        } else {
                            Poll::Pending
                        }
                    })
                    .await?;
                }
                Err(e) => return Err(e),
            }
        }
    }
}
