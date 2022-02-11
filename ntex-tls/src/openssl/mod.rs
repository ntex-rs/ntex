#![allow(clippy::type_complexity)]
//! An implementation of SSL streams for ntex backed by OpenSSL
use std::cell::{Cell, RefCell};
use std::{
    any, cmp, error::Error, future::Future, io, pin::Pin, task::Context, task::Poll,
};

use ntex_bytes::{BufMut, BytesVec, PoolRef};
use ntex_io::{Base, Filter, FilterFactory, Io, IoRef, ReadStatus, WriteStatus};
use ntex_util::{future::poll_fn, ready, time, time::Millis};
use tls_openssl::ssl::{self, SslStream};
use tls_openssl::x509::X509;

mod accept;
pub use self::accept::{Acceptor, AcceptorService};

use super::types;

/// Connection's peer cert
#[derive(Debug)]
pub struct PeerCert(pub X509);

/// Connection's peer cert chain
#[derive(Debug)]
pub struct PeerCertChain(pub Vec<X509>);

/// An implementation of SSL streams
pub struct SslFilter<F = Base> {
    inner: RefCell<SslStream<IoInner<F>>>,
    pool: PoolRef,
    handshake: Cell<bool>,
    read_buf: Cell<Option<BytesVec>>,
}

struct IoInner<F> {
    inner: F,
    pool: PoolRef,
    write_buf: Option<BytesVec>,
}

impl<F: Filter> io::Read for IoInner<F> {
    fn read(&mut self, dst: &mut [u8]) -> io::Result<usize> {
        if let Some(mut buf) = self.inner.get_read_buf() {
            if buf.is_empty() {
                Err(io::Error::from(io::ErrorKind::WouldBlock))
            } else {
                let len = cmp::min(buf.len(), dst.len());
                dst[..len].copy_from_slice(&buf.split_to(len));
                self.inner.release_read_buf(buf);
                Ok(len)
            }
        } else {
            Err(io::Error::from(io::ErrorKind::WouldBlock))
        }
    }
}

impl<F: Filter> io::Write for IoInner<F> {
    fn write(&mut self, src: &[u8]) -> io::Result<usize> {
        let mut buf = if let Some(mut buf) = self.inner.get_write_buf() {
            buf.reserve(src.len());
            buf
        } else {
            BytesVec::with_capacity_in(src.len(), self.pool)
        };
        buf.extend_from_slice(src);
        self.inner.release_write_buf(buf)?;
        Ok(src.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<F: Filter> Filter for SslFilter<F> {
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
        } else {
            self.inner.borrow().get_ref().inner.query(id)
        }
    }

    fn poll_shutdown(&self) -> Poll<io::Result<()>> {
        let ssl_result = self.inner.borrow_mut().shutdown();
        match ssl_result {
            Ok(ssl::ShutdownResult::Sent) => Poll::Pending,
            Ok(ssl::ShutdownResult::Received) => {
                self.inner.borrow().get_ref().inner.poll_shutdown()
            }
            Err(ref e) if e.code() == ssl::ErrorCode::ZERO_RETURN => {
                self.inner.borrow().get_ref().inner.poll_shutdown()
            }
            Err(ref e)
                if e.code() == ssl::ErrorCode::WANT_READ
                    || e.code() == ssl::ErrorCode::WANT_WRITE =>
            {
                Poll::Pending
            }
            Err(e) => Poll::Ready(Err(e
                .into_io_error()
                .unwrap_or_else(|e| io::Error::new(io::ErrorKind::Other, e)))),
        }
    }

    #[inline]
    fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<ReadStatus> {
        self.inner.borrow().get_ref().inner.poll_read_ready(cx)
    }

    #[inline]
    fn poll_write_ready(&self, cx: &mut Context<'_>) -> Poll<WriteStatus> {
        self.inner.borrow().get_ref().inner.poll_write_ready(cx)
    }

    #[inline]
    fn get_read_buf(&self) -> Option<BytesVec> {
        self.read_buf.take()
    }

    #[inline]
    fn get_write_buf(&self) -> Option<BytesVec> {
        self.inner.borrow_mut().get_mut().write_buf.take()
    }

    #[inline]
    fn release_read_buf(&self, buf: BytesVec) {
        self.read_buf.set(Some(buf));
    }

    fn process_read_buf(&self, io: &IoRef, nbytes: usize) -> io::Result<(usize, usize)> {
        // ask inner filter to process read buf
        match self
            .inner
            .borrow_mut()
            .get_ref()
            .inner
            .process_read_buf(io, nbytes)
        {
            Err(err) => io.want_shutdown(Some(err)),
            Ok((n, 0)) => return Ok((n, 0)),
            Ok((_, _)) => (),
        }

        // get processed buffer
        let mut dst = if let Some(dst) = self.get_read_buf() {
            dst
        } else {
            self.pool.get_read_buf()
        };
        let (hw, lw) = self.pool.read_params().unpack();

        let mut new_bytes = if self.handshake.get() { 1 } else { 0 };
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
                    Ok((dst.len(), new_bytes))
                }
                Err(ref e) if e.code() == ssl::ErrorCode::ZERO_RETURN => {
                    io.want_shutdown(None);
                    Ok((dst.len(), new_bytes))
                }
                Err(e) => Err(map_to_ioerr(e)),
            };
            self.release_read_buf(dst);
            return result;
        }
    }

    fn release_write_buf(&self, mut buf: BytesVec) -> Result<(), io::Error> {
        loop {
            if buf.is_empty() {
                return Ok(());
            }
            let ssl_result = self.inner.borrow_mut().ssl_write(&buf);
            match ssl_result {
                Ok(v) => {
                    buf.split_to(v);
                    continue;
                }
                Err(e) => {
                    if !buf.is_empty() {
                        self.inner.borrow_mut().get_mut().write_buf = Some(buf);
                    }
                    return match e.code() {
                        ssl::ErrorCode::WANT_READ | ssl::ErrorCode::WANT_WRITE => Ok(()),
                        _ => Err(map_to_ioerr(e)),
                    };
                }
            }
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
    type Filter = SslFilter<F>;

    type Error = Box<dyn Error>;
    type Future = Pin<Box<dyn Future<Output = Result<Io<Self::Filter>, Self::Error>>>>;

    fn create(self, st: Io<F>) -> Self::Future {
        let timeout = self.timeout;
        let ctx_result = ssl::Ssl::new(self.acceptor.context());

        Box::pin(async move {
            time::timeout(timeout, async {
                let ssl = ctx_result.map_err(map_to_ioerr)?;
                let pool = st.memory_pool();
                let st = st.map_filter(|inner: F| {
                    let inner = IoInner {
                        pool,
                        inner,
                        write_buf: None,
                    };
                    let ssl_stream = ssl::SslStream::new(ssl, inner)?;

                    Ok::<_, Box<dyn Error>>(SslFilter {
                        pool,
                        read_buf: Cell::new(None),
                        handshake: Cell::new(true),
                        inner: RefCell::new(ssl_stream),
                    })
                })?;

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
    type Filter = SslFilter<F>;

    type Error = Box<dyn Error>;
    type Future = Pin<Box<dyn Future<Output = Result<Io<Self::Filter>, Self::Error>>>>;

    fn create(self, st: Io<F>) -> Self::Future {
        Box::pin(async move {
            let ssl = self.ssl;
            let pool = st.memory_pool();
            let st = st.map_filter(|inner: F| {
                let inner = IoInner {
                    pool,
                    inner,
                    write_buf: None,
                };
                let ssl_stream = ssl::SslStream::new(ssl, inner)?;

                Ok::<_, Box<dyn Error>>(SslFilter {
                    pool,
                    read_buf: Cell::new(None),
                    handshake: Cell::new(true),
                    inner: RefCell::new(ssl_stream),
                })
            })?;

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
