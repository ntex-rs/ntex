//! An implementation of SSL streams for ntex backed by OpenSSL
use std::io::{self, Read as IoRead, Write as IoWrite};
use std::sync::Arc;
use std::{any, cell::RefCell, cmp, task::Context, task::Poll};

use ntex_bytes::{BufMut, BytesMut, PoolRef};
use ntex_io::{Filter, Io, IoRef, ReadFilter, WriteFilter, WriteReadiness};
use ntex_util::{future::poll_fn, time, time::Millis};
use tls_rust::{ServerConfig, ServerConnection};

use super::TlsFilter;
use crate::types;

/// An implementation of SSL streams
pub struct TlsServerFilter<F> {
    inner: RefCell<IoInner<F>>,
    session: RefCell<ServerConnection>,
}

struct IoInner<F> {
    inner: F,
    pool: PoolRef,
    read_buf: Option<BytesMut>,
    write_buf: Option<BytesMut>,
}

impl<F: Filter> Filter for TlsServerFilter<F> {
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
}

impl<F: Filter> ReadFilter for TlsServerFilter<F> {
    fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), ()>> {
        self.inner.borrow().inner.poll_read_ready(cx)
    }

    fn read_closed(&self, err: Option<io::Error>) {
        self.inner.borrow().inner.read_closed(err)
    }

    fn get_read_buf(&self) -> Option<BytesMut> {
        if let Some(buf) = self.inner.borrow_mut().read_buf.take() {
            if !buf.is_empty() {
                return Some(buf);
            }
        }
        None
    }

    fn release_read_buf(
        &self,
        mut src: BytesMut,
        _nb: usize,
    ) -> Result<bool, io::Error> {
        let mut session = self.session.borrow_mut();
        if session.is_handshaking() {
            self.inner.borrow_mut().read_buf = Some(src);
            Ok(false)
        } else {
            if src.is_empty() {
                return Ok(false);
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
}

impl<F: Filter> WriteFilter for TlsServerFilter<F> {
    fn poll_write_ready(
        &self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), WriteReadiness>> {
        self.inner.borrow().inner.poll_write_ready(cx)
    }

    fn write_closed(&self, err: Option<io::Error>) {
        self.inner.borrow().inner.read_closed(err)
    }

    fn get_write_buf(&self) -> Option<BytesMut> {
        if let Some(buf) = self.inner.borrow_mut().write_buf.take() {
            if !buf.is_empty() {
                return Some(buf);
            }
        }
        None
    }

    fn release_write_buf(&self, mut src: BytesMut) -> Result<bool, io::Error> {
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

        Ok(false)
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

impl<F: Filter> TlsServerFilter<F> {
    pub(crate) async fn create(
        st: Io<F>,
        cfg: Arc<ServerConfig>,
        timeout: Millis,
    ) -> Result<Io<TlsFilter<F>>, io::Error> {
        time::timeout(timeout, async {
            let pool = st.memory_pool();
            let session = match ServerConnection::new(cfg) {
                Ok(session) => session,
                Err(error) => return Err(io::Error::new(io::ErrorKind::Other, error)),
            };
            let st = st.map_filter(|inner: F| {
                let inner = IoInner {
                    pool,
                    inner,
                    read_buf: None,
                    write_buf: None,
                };

                Ok::<_, io::Error>(TlsFilter::new_server(TlsServerFilter {
                    inner: RefCell::new(inner),
                    session: RefCell::new(session),
                }))
            })?;

            let filter = st.filter();
            let read = st.read();

            loop {
                let (result, wants_read) = {
                    let mut session = filter.server().session.borrow_mut();
                    let mut inner = filter.server().inner.borrow_mut();
                    let mut io = Wrapper(&mut *inner);
                    let result = session.complete_io(&mut io);
                    let wants_read = session.wants_read();

                    if session.wants_write() {
                        loop {
                            let n = session.write_tls(&mut io)?;
                            if n == 0 {
                                break;
                            }
                        }
                    }
                    if result.is_ok() && wants_read {
                        poll_fn(|cx| {
                            read.poll_read_ready(cx).map_err(|e| {
                                e.unwrap_or_else(|| {
                                    io::Error::new(io::ErrorKind::Other, "disconnected")
                                })
                            })?;
                            Poll::Ready(Ok::<_, io::Error>(()))
                        })
                        .await?;
                    }
                    (result, wants_read)
                };
                match result {
                    Ok(_) => return Ok(st),
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                        if wants_read {
                            read.take_readiness();
                        }
                        poll_fn(|cx| {
                            let read_ready = if wants_read {
                                if read.is_ready() {
                                    true
                                } else {
                                    read.poll_read_ready(cx).map_err(|e| {
                                        e.unwrap_or_else(|| {
                                            io::Error::new(
                                                io::ErrorKind::Other,
                                                "disconnected",
                                            )
                                        })
                                    })?;
                                    false
                                }
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
