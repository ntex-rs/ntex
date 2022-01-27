//! An implementation of SSL streams for ntex backed by OpenSSL
use std::io::{self, Read as IoRead, Write as IoWrite};
use std::{any, cell::RefCell, sync::Arc, task::Context, task::Poll};

use ntex_bytes::{BufMut, BytesMut};
use ntex_io::{Filter, Io, IoRef, ReadStatus, WriteStatus};
use ntex_util::{future::poll_fn, ready};
use tls_rust::{ClientConfig, ClientConnection, ServerName};

use crate::rustls::{IoInner, TlsFilter, Wrapper};
use crate::types;

use super::{PeerCert, PeerCertChain};

/// An implementation of SSL streams
pub struct TlsClientFilter<F> {
    inner: RefCell<IoInner<F>>,
    session: RefCell<ClientConnection>,
}

impl<F: Filter> Filter for TlsClientFilter<F> {
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
            self.inner.borrow().filter.query(id)
        }
    }

    #[inline]
    fn poll_shutdown(&self) -> Poll<io::Result<()>> {
        self.inner.borrow().filter.poll_shutdown()
    }

    #[inline]
    fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<ReadStatus> {
        self.inner.borrow().filter.poll_read_ready(cx)
    }

    #[inline]
    fn poll_write_ready(&self, cx: &mut Context<'_>) -> Poll<WriteStatus> {
        self.inner.borrow().filter.poll_write_ready(cx)
    }

    #[inline]
    fn get_read_buf(&self) -> Option<BytesMut> {
        self.inner.borrow_mut().read_buf.take()
    }

    #[inline]
    fn get_write_buf(&self) -> Option<BytesMut> {
        self.inner.borrow_mut().write_buf.take()
    }

    #[inline]
    fn release_read_buf(&self, buf: BytesMut) {
        self.inner.borrow_mut().read_buf = Some(buf);
    }

    fn process_read_buf(&self, io: &IoRef, nbytes: usize) -> io::Result<(usize, usize)> {
        let mut inner = self.inner.borrow_mut();
        let mut session = self.session.borrow_mut();

        // ask inner filter to process read buf
        match inner.filter.process_read_buf(io, nbytes) {
            Err(err) => io.want_shutdown(Some(err)),
            Ok((_, 0)) => return Ok((0, 0)),
            Ok(_) => (),
        }

        if session.is_handshaking() {
            Ok((0, 1))
        } else {
            // get processed buffer
            let mut dst = if let Some(dst) = inner.read_buf.take() {
                dst
            } else {
                inner.pool.get_read_buf()
            };
            let (hw, lw) = inner.pool.read_params().unpack();

            let mut src = if let Some(src) = inner.filter.get_read_buf() {
                src
            } else {
                return Ok((0, 0));
            };

            let mut new_bytes = 0;
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

            let dst_len = dst.len();
            inner.read_buf = Some(dst);
            inner.filter.release_read_buf(src);
            Ok((dst_len, new_bytes))
        }
    }

    fn release_write_buf(&self, mut src: BytesMut) -> Result<(), io::Error> {
        let mut session = self.session.borrow_mut();
        let mut inner = self.inner.borrow_mut();
        let mut io = Wrapper(&mut *inner);

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
            self.inner.borrow_mut().write_buf = Some(src);
        }

        Ok(())
    }
}

impl<F: Filter> TlsClientFilter<F> {
    pub(crate) async fn create(
        io: Io<F>,
        cfg: Arc<ClientConfig>,
        domain: ServerName,
    ) -> Result<Io<TlsFilter<F>>, io::Error> {
        let pool = io.memory_pool();
        let session = match ClientConnection::new(cfg, domain) {
            Ok(session) => session,
            Err(error) => return Err(io::Error::new(io::ErrorKind::Other, error)),
        };
        let io = io.map_filter(|filter: F| {
            let inner = IoInner {
                pool,
                filter,
                read_buf: None,
                write_buf: None,
            };

            Ok::<_, io::Error>(TlsFilter::new_client(TlsClientFilter {
                inner: RefCell::new(inner),
                session: RefCell::new(session),
            }))
        })?;

        let filter = io.filter();
        loop {
            let (result, wants_read) = {
                let mut session = filter.client().session.borrow_mut();
                let mut inner = filter.client().inner.borrow_mut();
                let mut wrp = Wrapper(&mut *inner);
                let result = session.complete_io(&mut wrp);
                let wants_read = session.wants_read();

                if session.wants_write() {
                    loop {
                        let n = session.write_tls(&mut wrp)?;
                        if n == 0 {
                            break;
                        }
                    }
                }
                (result, wants_read)
            };
            if result.is_ok() && wants_read {
                poll_fn(|cx| match ready!(io.poll_read_ready(cx)) {
                    Ok(None) => Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::Other,
                        "disconnected",
                    ))),
                    Err(e) => Poll::Ready(Err(e)),
                    _ => Poll::Ready(Ok(())),
                })
                .await?;
            }
            match result {
                Ok(_) => return Ok(io),
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
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
