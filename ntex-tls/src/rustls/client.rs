//! An implementation of SSL streams for ntex backed by OpenSSL
use std::io::{self, Read as IoRead, Write as IoWrite};
use std::{any, cell::Cell, cell::RefCell, sync::Arc, task::Poll};

use ntex_bytes::BufMut;
use ntex_io::{types, Filter, FilterLayer, Io, Layer, ReadBuf, WriteBuf};
use ntex_util::{future::poll_fn, ready};
use tls_rust::{ClientConfig, ClientConnection, ServerName};

use crate::rustls::{IoInner, TlsFilter, Wrapper};

use super::{PeerCert, PeerCertChain};

/// An implementation of SSL streams
pub struct TlsClientFilter {
    inner: IoInner,
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

    fn process_read_buf(&self, buf: &mut ReadBuf<'_>) -> io::Result<usize> {
        let mut session = self.session.borrow_mut();

        // get processed buffer
        let (dst, src) = buf.get_pair();
        let (hw, lw) = self.inner.pool.read_params().unpack();

        let mut new_bytes = usize::from(self.inner.handshake.get());
        loop {
            // make sure we've got room
            let remaining = dst.remaining_mut();
            if remaining < lw {
                dst.reserve(hw - remaining);
            }

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

                let n = session.write_tls(&mut io)?;
                if n == 0 {
                    break;
                }
            }

            if !src.is_empty() {
                buf.set_src(Some(src));
            }
            Ok(())
        } else {
            Ok(())
        }
    }
}

impl TlsClientFilter {
    pub(crate) async fn create<F: Filter>(
        io: Io<F>,
        cfg: Arc<ClientConfig>,
        domain: ServerName,
    ) -> Result<Io<Layer<TlsFilter, F>>, io::Error> {
        let pool = io.memory_pool();
        let session = match ClientConnection::new(cfg, domain) {
            Ok(session) => session,
            Err(error) => return Err(io::Error::new(io::ErrorKind::Other, error)),
        };
        let inner = IoInner {
            pool,
            handshake: Cell::new(true),
        };
        let filter = TlsFilter::new_client(TlsClientFilter {
            inner,
            session: RefCell::new(session),
        });
        let io = io.add_filter(filter);

        let filter = io.filter();
        loop {
            let (result, wants_read, handshaking) = io.with_buf(|buf| {
                let mut session = filter.client().session.borrow_mut();
                let mut wrp = Wrapper(&filter.client().inner, buf);
                (
                    session.complete_io(&mut wrp),
                    session.wants_read(),
                    session.is_handshaking(),
                )
            })?;
            match result {
                Ok(_) => {
                    filter.client().inner.handshake.set(false);
                    return Ok(io);
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if !handshaking {
                        filter.client().inner.handshake.set(false);
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
    }
}
