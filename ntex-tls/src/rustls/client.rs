//! An implementation of SSL streams for ntex backed by OpenSSL
use std::io::{self, Read as IoRead, Write as IoWrite};
use std::sync::Arc;
use std::{any, cell::RefCell, cmp, task::Context, task::Poll};

use ntex_bytes::{BufMut, BytesMut, PoolRef};
use ntex_io::{Filter, Io, IoRef, WriteReadiness};
use ntex_util::{future::poll_fn, ready};
use tls_rust::{ClientConfig, ClientConnection, ServerName};

use super::TlsFilter;
use crate::types;

/// An implementation of SSL streams
pub struct TlsClientFilter<F> {
    inner: RefCell<IoInner<F>>,
    session: RefCell<ClientConnection>,
}

struct IoInner<F> {
    inner: F,
    pool: PoolRef,
    read_buf: Option<BytesMut>,
    write_buf: Option<BytesMut>,
}

impl<F: Filter> Filter for TlsClientFilter<F> {
    #[inline]
    fn shutdown(&self, st: &IoRef) -> Poll<Result<(), io::Error>> {
        self.inner.borrow().inner.shutdown(st)
    }

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
        } else {
            self.inner.borrow().inner.query(id)
        }
    }

    #[inline]
    fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), ()>> {
        self.inner.borrow().inner.poll_read_ready(cx)
    }

    #[inline]
    fn poll_write_ready(
        &self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), WriteReadiness>> {
        self.inner.borrow().inner.poll_write_ready(cx)
    }

    #[inline]
    fn closed(&self, err: Option<io::Error>) {
        self.inner.borrow().inner.closed(err)
    }

    #[inline]
    fn get_read_buf(&self) -> Option<BytesMut> {
        if let Some(buf) = self.inner.borrow_mut().read_buf.take() {
            if !buf.is_empty() {
                return Some(buf);
            }
        }
        None
    }

    #[inline]
    fn get_write_buf(&self) -> Option<BytesMut> {
        if let Some(buf) = self.inner.borrow_mut().write_buf.take() {
            if !buf.is_empty() {
                return Some(buf);
            }
        }
        None
    }

    fn release_read_buf(&self, mut src: BytesMut, _nb: usize) -> Result<(), io::Error> {
        let mut session = self.session.borrow_mut();
        if session.is_handshaking() {
            self.inner.borrow_mut().read_buf = Some(src);
            Ok(())
        } else {
            if src.is_empty() {
                return Ok(());
            }
            let mut inner = self.inner.borrow_mut();
            let (hw, lw) = inner.pool.read_params().unpack();

            // get inner filter buffer
            let mut buf = if let Some(buf) = inner.inner.get_read_buf() {
                buf
            } else {
                BytesMut::with_capacity_in(lw, inner.pool)
            };

            let mut new_bytes = 0;
            loop {
                // make sure we've got room
                let remaining = buf.remaining_mut();
                if remaining < lw {
                    buf.reserve(hw - remaining);
                }

                let mut cursor = io::Cursor::new(&src);
                let n = session.read_tls(&mut cursor)?;
                src.split_to(n);
                let state = session
                    .process_new_packets()
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

                let new_b = state.plaintext_bytes_to_read();
                if new_b > 0 {
                    buf.reserve(new_b);
                    let chunk: &mut [u8] =
                        unsafe { std::mem::transmute(&mut *buf.chunk_mut()) };
                    let v = session.reader().read(chunk)?;
                    unsafe { buf.advance_mut(v) };
                    new_bytes += v;
                } else {
                    break;
                }
            }

            if !src.is_empty() {
                inner.read_buf = Some(src);
            }
            inner.inner.release_read_buf(buf, new_bytes)
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

struct Wrapper<'a, F>(&'a mut IoInner<F>);

impl<'a, F: Filter> io::Read for Wrapper<'a, F> {
    fn read(&mut self, dst: &mut [u8]) -> io::Result<usize> {
        if let Some(read_buf) = self.0.read_buf.as_mut() {
            let len = cmp::min(read_buf.len(), dst.len());
            if len > 0 {
                dst[..len].copy_from_slice(&read_buf.split_to(len));
                Ok(len)
            } else {
                Err(io::Error::new(io::ErrorKind::WouldBlock, ""))
            }
        } else {
            Err(io::Error::new(io::ErrorKind::WouldBlock, ""))
        }
    }
}

impl<'a, F: Filter> io::Write for Wrapper<'a, F> {
    fn write(&mut self, src: &[u8]) -> io::Result<usize> {
        let mut buf = if let Some(mut buf) = self.0.inner.get_write_buf() {
            buf.reserve(src.len());
            buf
        } else {
            BytesMut::with_capacity_in(src.len(), self.0.pool)
        };
        buf.extend_from_slice(src);
        self.0.inner.release_write_buf(buf)?;
        Ok(src.len())
    }

    fn flush(&mut self) -> io::Result<()> {
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
        let io = io.map_filter(|inner: F| {
            let inner = IoInner {
                pool,
                inner,
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
