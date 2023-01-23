//! An implementation of SSL streams for ntex backed by OpenSSL
use std::io::{self, Read as IoRead, Write as IoWrite};
use std::{any, cell::Cell, cell::RefCell, sync::Arc, task::Poll};

use ntex_bytes::BufMut;
use ntex_io::{types, Filter, FilterLayer, Io, Layer, ReadBuf, WriteBuf};
use ntex_util::{future::poll_fn, ready, time, time::Millis};
use tls_rust::{ServerConfig, ServerConnection};

use crate::rustls::{IoInner, TlsFilter, Wrapper};
use crate::Servername;

use super::{PeerCert, PeerCertChain};

/// An implementation of SSL streams
pub struct TlsServerFilter {
    inner: IoInner,
    session: RefCell<ServerConnection>,
}

impl FilterLayer for TlsServerFilter {
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
        } else if id == any::TypeId::of::<Servername>() {
            if let Some(name) = self.session.borrow().sni_hostname() {
                Some(Box::new(Servername(name.to_string())))
            } else {
                None
            }
        } else {
            None
        }
    }

    fn process_read_buf(&self, buf: &mut ReadBuf<'_>) -> io::Result<usize> {
        let mut session = self.session.borrow_mut();

        // get processed buffer
        let (src, dst) = buf.get_pair();
        let mut new_bytes = usize::from(self.inner.handshake.get());
        loop {
            // make sure we've got room
            self.inner.pool.resize_read_buf(dst);

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

        Ok(new_bytes)
    }

    fn process_write_buf(&self, buf: &mut WriteBuf<'_>) -> io::Result<()> {
        if let Some(mut src) = buf.take_src() {
            let mut session = self.session.borrow_mut();
            let mut io = Wrapper(&self.inner, buf);

            loop {
                if !src.is_empty() {
                    let n = session.writer().write(&src)?;
                    src.split_to(n);
                }

                if session.wants_write() {
                    session.complete_io(&mut io)?;
                } else {
                    break;
                }
            }

            if !src.is_empty() {
                buf.set_src(Some(src));
            }
        }
        Ok(())
    }
}

impl TlsServerFilter {
    pub(crate) async fn create<F: Filter>(
        io: Io<F>,
        cfg: Arc<ServerConfig>,
        timeout: Millis,
    ) -> Result<Io<Layer<TlsFilter, F>>, io::Error> {
        time::timeout(timeout, async {
            let session = ServerConnection::new(cfg)
                .map_err(|err| io::Error::new(io::ErrorKind::Other, err))?;
            let filter = TlsFilter::new_server(TlsServerFilter {
                session: RefCell::new(session),
                inner: IoInner {
                    pool: io.memory_pool(),
                    handshake: Cell::new(true),
                },
            });
            let io = io.add_filter(filter);

            let filter = io.filter();
            loop {
                let (result, wants_read, handshaking) = io.with_buf(|buf| {
                    let mut session = filter.server().session.borrow_mut();
                    let mut wrp = Wrapper(&filter.server().inner, buf);
                    let mut result = (
                        session.complete_io(&mut wrp),
                        session.wants_read(),
                        session.is_handshaking(),
                    );

                    while session.wants_write() {
                        result.0 = session.complete_io(&mut wrp);
                        if result.0.is_err() {
                            break;
                        }
                    }
                    result
                })?;

                match result {
                    Ok(_) => {
                        filter.server().inner.handshake.set(false);
                        return Ok(io);
                    }
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                        if !handshaking {
                            filter.server().inner.handshake.set(false);
                            return Ok(io);
                        }
                        poll_fn(|cx| {
                            let read_ready = if wants_read {
                                match ready!(io.poll_read_ready(cx))? {
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
        })
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "rustls handshake timeout"))
        .and_then(|item| item)
    }
}
