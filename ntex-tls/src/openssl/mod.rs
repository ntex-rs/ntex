#![allow(clippy::type_complexity)]
//! An implementation of SSL streams for ntex backed by OpenSSL
use std::cell::{Cell, RefCell};
use std::{any, cmp, error::Error, io, task::Context, task::Poll};

use ntex_bytes::{BufMut, BytesVec, PoolRef};
use ntex_io::{types, Filter, FilterFactory, FilterLayer, Io, Layer, ReadBuf, WriteBuf};
use ntex_util::{future::poll_fn, future::BoxFuture, ready, time, time::Millis};
use tls_openssl::ssl::{self, NameType, SslStream};
use tls_openssl::x509::X509;

use crate::{PskIdentity, Servername};

mod accept;
pub use self::accept::{Acceptor, AcceptorService};

/// Connection's peer cert
#[derive(Debug)]
pub struct PeerCert(pub X509);

/// Connection's peer cert chain
#[derive(Debug)]
pub struct PeerCertChain(pub Vec<X509>);

/// An implementation of SSL streams
pub struct SslFilter {
    inner: RefCell<SslStream<IoInner>>,
    pool: PoolRef,
    handshake: Cell<bool>,
}

struct IoInner {
    inner_read_buf: Option<BytesVec>,
    inner_write_buf: Option<BytesVec>,
    pool: PoolRef,
}

impl io::Read for IoInner {
    fn read(&mut self, dst: &mut [u8]) -> io::Result<usize> {
        if let Some(mut buf) = self.inner_read_buf.take() {
            if buf.is_empty() {
                Err(io::Error::from(io::ErrorKind::WouldBlock))
            } else {
                let len = cmp::min(buf.len(), dst.len());
                dst[..len].copy_from_slice(&buf.split_to(len));
                self.inner_read_buf = Some(buf);
                Ok(len)
            }
        } else {
            Err(io::Error::from(io::ErrorKind::WouldBlock))
        }
    }
}

impl io::Write for IoInner {
    fn write(&mut self, src: &[u8]) -> io::Result<usize> {
        let mut buf = if let Some(mut buf) = self.inner_write_buf.take() {
            buf.reserve(src.len());
            buf
        } else {
            BytesVec::with_capacity_in(src.len(), self.pool)
        };
        buf.extend_from_slice(src);
        self.inner_write_buf = Some(buf);
        Ok(src.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl FilterLayer for SslFilter {
    fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>> {
        const H2: &[u8] = b"h2";

        if id == any::TypeId::of::<types::HttpProtocol>() {
            let h2 = self
                .inner
                .borrow()
                .ssl()
                .selected_alpn_protocol()
                .map(|protos| protos.windows(2).any(|w| w == H2))
                .unwrap_or(false);
            let proto = if h2 {
                types::HttpProtocol::Http2
            } else {
                types::HttpProtocol::Http1
            };
            Some(Box::new(proto))
        } else if id == any::TypeId::of::<PeerCert>() {
            if let Some(cert) = self.inner.borrow().ssl().peer_certificate() {
                Some(Box::new(PeerCert(cert)))
            } else {
                None
            }
        } else if id == any::TypeId::of::<PeerCertChain>() {
            if let Some(cert_chain) = self.inner.borrow().ssl().peer_cert_chain() {
                Some(Box::new(PeerCertChain(
                    cert_chain.iter().map(|c| c.to_owned()).collect(),
                )))
            } else {
                None
            }
        } else if id == any::TypeId::of::<Servername>() {
            if let Some(name) = self.inner.borrow().ssl().servername(NameType::HOST_NAME) {
                Some(Box::new(Servername(name.to_string())))
            } else {
                None
            }
        } else if id == any::TypeId::of::<PskIdentity>() {
            if let Some(psk_id) = self.inner.borrow().ssl().psk_identity() {
                Some(Box::new(PskIdentity(psk_id.to_vec())))
            } else {
                None
            }
        } else {
            None
        }
    }

    fn shutdown(&self, buf: &mut WriteBuf<'_>) -> io::Result<Poll<()>> {
        self.inner.borrow_mut().get_mut().inner_write_buf = Some(buf.take_dst());
        self.inner.borrow_mut().get_mut().inner_read_buf =
            buf.with_read_buf(|b| b.take_src());
        let ssl_result = self.inner.borrow_mut().shutdown();
        buf.set_dst(self.inner.borrow_mut().get_mut().inner_write_buf.take());
        buf.with_read_buf(|b| {
            b.set_src(self.inner.borrow_mut().get_mut().inner_read_buf.take())
        });

        match ssl_result {
            Ok(ssl::ShutdownResult::Sent) => Ok(Poll::Pending),
            Ok(ssl::ShutdownResult::Received) => Ok(Poll::Ready(())),
            Err(ref e) if e.code() == ssl::ErrorCode::ZERO_RETURN => Ok(Poll::Ready(())),
            Err(ref e)
                if e.code() == ssl::ErrorCode::WANT_READ
                    || e.code() == ssl::ErrorCode::WANT_WRITE =>
            {
                Ok(Poll::Pending)
            }
            Err(e) => Err(e
                .into_io_error()
                .unwrap_or_else(|e| io::Error::new(io::ErrorKind::Other, e))),
        }
    }

    fn process_read_buf(&self, buf: &mut ReadBuf<'_>) -> io::Result<usize> {
        // get processed buffer
        self.inner.borrow_mut().get_mut().inner_read_buf = buf.take_src();

        let dst = buf.get_dst();
        let (hw, lw) = self.pool.read_params().unpack();

        let mut new_bytes = usize::from(self.handshake.get());
        loop {
            // make sure we've got room
            let remaining = dst.remaining_mut();
            if remaining < lw {
                dst.reserve(hw - remaining);
            }

            let chunk: &mut [u8] = unsafe { std::mem::transmute(&mut *dst.chunk_mut()) };
            let ssl_result = self.inner.borrow_mut().ssl_read(chunk);
            let result = match ssl_result {
                Ok(v) => {
                    unsafe { dst.advance_mut(v) };
                    new_bytes += v;
                    continue;
                }
                Err(ref e)
                    if e.code() == ssl::ErrorCode::WANT_READ
                        || e.code() == ssl::ErrorCode::WANT_WRITE =>
                {
                    Ok(new_bytes)
                }
                Err(ref e) if e.code() == ssl::ErrorCode::ZERO_RETURN => {
                    buf.io().want_shutdown(None);
                    Ok(new_bytes)
                }
                Err(e) => {
                    log::trace!("SSL Error: {:?}", e);
                    Err(map_to_ioerr(e))
                }
            };

            buf.set_src(self.inner.borrow_mut().get_mut().inner_read_buf.take());
            return result;
        }
    }

    fn process_write_buf(&self, buf: &mut WriteBuf<'_>) -> io::Result<()> {
        if let Some(mut src) = buf.take_src() {
            // get processed buffer
            self.inner.borrow_mut().get_mut().inner_write_buf = Some(buf.take_dst());

            loop {
                let ssl_result = self.inner.borrow_mut().ssl_write(&src);
                match ssl_result {
                    Ok(v) => {
                        src.split_to(v);
                        continue;
                    }
                    Err(e) => {
                        if !src.is_empty() {
                            buf.set_src(Some(src));
                        }
                        return match e.code() {
                            ssl::ErrorCode::WANT_READ | ssl::ErrorCode::WANT_WRITE => {
                                buf.set_dst(
                                    self.inner
                                        .borrow_mut()
                                        .get_mut()
                                        .inner_write_buf
                                        .take(),
                                );
                                Ok(())
                            }
                            _ => Err(map_to_ioerr(e)),
                        };
                    }
                }
            }
        } else {
            Ok(())
        }
    }
}

pub struct SslAcceptor {
    acceptor: ssl::SslAcceptor,
    timeout: Millis,
}

impl SslAcceptor {
    /// Create openssl acceptor filter factory
    pub fn new(acceptor: ssl::SslAcceptor) -> Self {
        SslAcceptor {
            acceptor,
            timeout: Millis(5_000),
        }
    }

    /// Set handshake timeout.
    ///
    /// Default is set to 5 seconds.
    pub fn timeout<U: Into<Millis>>(&mut self, timeout: U) -> &mut Self {
        self.timeout = timeout.into();
        self
    }
}

impl Clone for SslAcceptor {
    fn clone(&self) -> Self {
        Self {
            acceptor: self.acceptor.clone(),
            timeout: self.timeout,
        }
    }
}

impl<F: Filter> FilterFactory<F> for SslAcceptor {
    type Filter = SslFilter;

    type Error = Box<dyn Error>;
    type Future = BoxFuture<'static, Result<Io<Layer<Self::Filter, F>>, Self::Error>>;

    fn create(self, st: Io<F>) -> Self::Future {
        let timeout = self.timeout;
        let ctx_result = ssl::Ssl::new(self.acceptor.context());

        Box::pin(async move {
            time::timeout(timeout, async {
                let ssl = ctx_result.map_err(map_to_ioerr)?;
                let pool = st.memory_pool();
                let inner = IoInner {
                    pool,
                    inner_read_buf: None,
                    inner_write_buf: None,
                };
                let ssl_stream = ssl::SslStream::new(ssl, inner)?;

                let filter = SslFilter {
                    pool,
                    handshake: Cell::new(true),
                    inner: RefCell::new(ssl_stream),
                };
                let st = st.add_filter(filter);

                poll_fn(|cx| {
                    handle_result(st.filter().inner.borrow_mut().accept(), &st, cx)
                })
                .await?;

                st.filter().handshake.set(false);
                Ok(st)
            })
            .await
            .map_err(|_| {
                io::Error::new(io::ErrorKind::TimedOut, "ssl handshake timeout").into()
            })
            .and_then(|item| item)
        })
    }
}

pub struct SslConnector {
    ssl: ssl::Ssl,
}

impl SslConnector {
    /// Create openssl connector filter factory
    pub fn new(ssl: ssl::Ssl) -> Self {
        SslConnector { ssl }
    }
}

impl<F: Filter> FilterFactory<F> for SslConnector {
    type Filter = SslFilter;

    type Error = Box<dyn Error>;
    type Future = BoxFuture<'static, Result<Io<Layer<Self::Filter, F>>, Self::Error>>;

    fn create(self, st: Io<F>) -> Self::Future {
        Box::pin(async move {
            let ssl = self.ssl;
            let pool = st.memory_pool();
            let inner = IoInner {
                pool,
                inner_read_buf: None,
                inner_write_buf: None,
            };
            let filter = SslFilter {
                pool,
                handshake: Cell::new(true),
                inner: RefCell::new(ssl::SslStream::new(ssl, inner)?),
            };
            let st = st.add_filter(filter);

            poll_fn(|cx| handle_result(st.filter().inner.borrow_mut().connect(), &st, cx))
                .await?;

            Ok(st)
        })
    }
}

fn handle_result<T, F>(
    result: Result<T, ssl::Error>,
    io: &Io<F>,
    cx: &mut Context<'_>,
) -> Poll<Result<T, Box<dyn Error>>> {
    match result {
        Ok(v) => Poll::Ready(Ok(v)),
        Err(e) => match e.code() {
            ssl::ErrorCode::WANT_READ => {
                match ready!(io.poll_read_ready(cx)) {
                    Ok(None) => Err::<_, Box<dyn Error>>(
                        io::Error::new(io::ErrorKind::Other, "disconnected").into(),
                    ),
                    Err(err) => Err(err.into()),
                    _ => Ok(()),
                }?;
                Poll::Pending
            }
            ssl::ErrorCode::WANT_WRITE => {
                let _ = io.poll_flush(cx, true)?;
                Poll::Pending
            }
            _ => Poll::Ready(Err(Box::new(e))),
        },
    }
}

fn map_to_ioerr<E: Into<Box<dyn Error + Send + Sync>>>(err: E) -> io::Error {
    io::Error::new(io::ErrorKind::Other, err)
}
