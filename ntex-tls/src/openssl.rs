#![allow(clippy::type_complexity)]
//! An implementation of SSL streams for ntex backed by OpenSSL
use std::cell::RefCell;
use std::{
    any, cmp, error::Error, future::Future, io, pin::Pin, task::Context, task::Poll,
};

use ntex_bytes::{BufMut, BytesMut, PoolRef};
use ntex_io::{
    Filter, FilterFactory, Io, IoRef, ReadFilter, WriteFilter, WriteReadiness,
};
use ntex_util::{future::poll_fn, time, time::Millis};
use tls_openssl::ssl::{self, SslStream};

use super::types;

/// An implementation of SSL streams
pub struct SslFilter<F> {
    inner: RefCell<SslStream<IoInner<F>>>,
}

struct IoInner<F> {
    inner: F,
    pool: PoolRef,
    read_buf: Option<BytesMut>,
    write_buf: Option<BytesMut>,
}

impl<F: Filter> io::Read for IoInner<F> {
    fn read(&mut self, dst: &mut [u8]) -> io::Result<usize> {
        if let Some(ref mut buf) = self.read_buf {
            if buf.is_empty() {
                buf.clear();
                Err(io::Error::from(io::ErrorKind::WouldBlock))
            } else {
                let len = cmp::min(buf.len(), dst.len());
                dst[..len].copy_from_slice(&buf.split_to(len));
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
            buf.reserve(buf.len());
            buf
        } else {
            BytesMut::with_capacity_in(src.len(), self.pool)
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
    fn shutdown(&self, st: &IoRef) -> Poll<Result<(), io::Error>> {
        let ssl_result = self.inner.borrow_mut().shutdown();
        match ssl_result {
            Ok(ssl::ShutdownResult::Sent) => Poll::Pending,
            Ok(ssl::ShutdownResult::Received) => {
                self.inner.borrow().get_ref().inner.shutdown(st)
            }
            Err(ref e) if e.code() == ssl::ErrorCode::ZERO_RETURN => Poll::Ready(Ok(())),
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

    fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>> {
        if id == any::TypeId::of::<types::HttpProtocol>() {
            let proto = if let Some(protos) =
                self.inner.borrow().ssl().selected_alpn_protocol()
            {
                if protos.windows(2).any(|window| window == b"h2") {
                    types::HttpProtocol::Http2
                } else {
                    types::HttpProtocol::Http1
                }
            } else {
                types::HttpProtocol::Http1
            };
            Some(Box::new(proto))
        } else {
            self.inner.borrow().get_ref().inner.query(id)
        }
    }
}

impl<F: Filter> ReadFilter for SslFilter<F> {
    fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), ()>> {
        self.inner.borrow().get_ref().inner.poll_read_ready(cx)
    }

    fn read_closed(&self, err: Option<io::Error>) {
        self.inner.borrow().get_ref().inner.read_closed(err)
    }

    fn get_read_buf(&self) -> Option<BytesMut> {
        if let Some(buf) = self.inner.borrow_mut().get_mut().read_buf.take() {
            if !buf.is_empty() {
                return Some(buf);
            }
        }
        None
    }

    fn release_read_buf(
        &self,
        src: BytesMut,
        new_bytes: usize,
    ) -> Result<(), io::Error> {
        // store to read_buf
        let pool = {
            let mut inner = self.inner.borrow_mut();
            inner.get_mut().read_buf = Some(src);
            inner.get_ref().pool
        };
        if new_bytes == 0 {
            return Ok(());
        }
        let (hw, lw) = pool.read_params().unpack();

        // get inner filter buffer
        let mut buf =
            if let Some(buf) = self.inner.borrow().get_ref().inner.get_read_buf() {
                buf
            } else {
                BytesMut::with_capacity_in(lw, pool)
            };

        let mut new_bytes = 0;
        loop {
            // make sure we've got room
            let remaining = buf.remaining_mut();
            if remaining < lw {
                buf.reserve(hw - remaining);
            }

            let chunk: &mut [u8] = unsafe { std::mem::transmute(&mut *buf.chunk_mut()) };
            let ssl_result = self.inner.borrow_mut().ssl_read(chunk);
            return match ssl_result {
                Ok(v) => {
                    unsafe { buf.advance_mut(v) };
                    new_bytes += v;
                    continue;
                }
                Err(ref e)
                    if e.code() == ssl::ErrorCode::WANT_READ
                        || e.code() == ssl::ErrorCode::WANT_WRITE =>
                {
                    self.inner
                        .borrow()
                        .get_ref()
                        .inner
                        .release_read_buf(buf, new_bytes)?;
                    Ok(())
                }
                Err(e) => Err(map_to_ioerr(e)),
            };
        }
    }
}

impl<F: Filter> WriteFilter for SslFilter<F> {
    fn poll_write_ready(
        &self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), WriteReadiness>> {
        self.inner.borrow().get_ref().inner.poll_write_ready(cx)
    }

    fn write_closed(&self, err: Option<io::Error>) {
        self.inner.borrow().get_ref().inner.read_closed(err)
    }

    fn get_write_buf(&self) -> Option<BytesMut> {
        if let Some(buf) = self.inner.borrow_mut().get_mut().write_buf.take() {
            if !buf.is_empty() {
                return Some(buf);
            }
        }
        None
    }

    fn release_write_buf(&self, mut buf: BytesMut) -> Result<(), io::Error> {
        let ssl_result = self.inner.borrow_mut().ssl_write(&buf);
        let result = match ssl_result {
            Ok(v) => {
                if v != buf.len() {
                    buf.split_to(v);
                    self.inner.borrow_mut().get_mut().write_buf = Some(buf);
                }
                Ok(())
            }
            Err(e) => match e.code() {
                ssl::ErrorCode::WANT_READ | ssl::ErrorCode::WANT_WRITE => Ok(()),
                _ => (Err(map_to_ioerr(e))),
            },
        };
        result
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

impl<F: Filter + 'static> FilterFactory<F> for SslAcceptor {
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
                let st = st.map_filter::<Self, _>(|inner: F| {
                    let inner = IoInner {
                        pool,
                        inner,
                        read_buf: None,
                        write_buf: None,
                    };
                    let ssl_stream = ssl::SslStream::new(ssl, inner)?;

                    Ok(SslFilter {
                        inner: RefCell::new(ssl_stream),
                    })
                })?;

                poll_fn(|cx| {
                    handle_result(st.filter().inner.borrow_mut().accept(), &st, cx)
                })
                .await?;

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

impl<F: Filter + 'static> FilterFactory<F> for SslConnector {
    type Filter = SslFilter<F>;

    type Error = Box<dyn Error>;
    type Future = Pin<Box<dyn Future<Output = Result<Io<Self::Filter>, Self::Error>>>>;

    fn create(self, st: Io<F>) -> Self::Future {
        Box::pin(async move {
            let ssl = self.ssl;
            let pool = st.memory_pool();
            let st = st.map_filter::<Self, _>(|inner: F| {
                let inner = IoInner {
                    pool,
                    inner,
                    read_buf: None,
                    write_buf: None,
                };
                let ssl_stream = ssl::SslStream::new(ssl, inner)?;

                Ok(SslFilter {
                    inner: RefCell::new(ssl_stream),
                })
            })?;

            poll_fn(|cx| {
                handle_result(st.filter().inner.borrow_mut().connect(), &st, cx)
            })
            .await?;

            Ok(st)
        })
    }
}

fn handle_result<T>(
    result: Result<T, ssl::Error>,
    st: &IoRef,
    cx: &mut Context<'_>,
) -> Poll<Result<T, Box<dyn Error>>> {
    match result {
        Ok(v) => Poll::Ready(Ok(v)),
        Err(e) => match e.code() {
            ssl::ErrorCode::WANT_READ => {
                if let Err(e) = st.read().poll_read_ready(cx) {
                    let e = e.unwrap_or_else(|| {
                        io::Error::new(io::ErrorKind::Other, "disconnected")
                    });
                    Poll::Ready(Err(e.into()))
                } else {
                    Poll::Pending
                }
            }
            ssl::ErrorCode::WANT_WRITE => {
                let _ = st.write().poll_write_ready(cx, true)?;
                Poll::Pending
            }
            _ => Poll::Ready(Err(Box::new(e))),
        },
    }
}

fn map_to_ioerr<E: Into<Box<dyn Error + Send + Sync>>>(err: E) -> io::Error {
    io::Error::new(io::ErrorKind::Other, err)
}
