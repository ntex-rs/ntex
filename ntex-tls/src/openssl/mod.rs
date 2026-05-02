//! An implementation of SSL streams for ntex backed by OpenSSL
use std::{any, borrow::ToOwned, cell::RefCell, cmp, error::Error, io, task::Poll};

use ntex_bytes::{BufMut, BytePages, BytesMut};
use ntex_io::{Filter, FilterBuf, FilterLayer, Io, Layer, types};
use tls_openssl::ssl::{self, NameType, SslStream};
use tls_openssl::x509::X509;

use crate::{PskIdentity, Servername};

mod connect;
pub use self::connect::{
    SslConnector, SslConnector2, SslConnectorService, SslConnectorService2,
};

mod accept;
pub use self::accept::{SslAcceptor, SslAcceptorService};

/// Connection's peer cert
#[derive(Debug)]
pub struct PeerCert(pub X509);

/// Connection's peer cert chain
#[derive(Debug)]
pub struct PeerCertChain(pub Vec<X509>);

/// An implementation of SSL streams
#[derive(Debug)]
pub struct SslFilter {
    inner: RefCell<SslStream<IoInner>>,
}

#[derive(Debug)]
struct IoInner {
    source: Option<BytesMut>,
    destination: BytePages,
    stopped: bool,
}

impl io::Read for IoInner {
    fn read(&mut self, dst: &mut [u8]) -> io::Result<usize> {
        if let Some(ref mut buf) = self.source {
            if buf.is_empty() {
                Err(io::Error::from(io::ErrorKind::WouldBlock))
            } else {
                let len = cmp::min(buf.len(), dst.len());
                dst[..len].copy_from_slice(&buf[..len]);
                buf.advance_to(len);
                Ok(len)
            }
        } else {
            Err(io::Error::from(io::ErrorKind::WouldBlock))
        }
    }
}

impl io::Write for IoInner {
    fn write(&mut self, src: &[u8]) -> io::Result<usize> {
        self.destination.extend_from_slice(src);
        Ok(src.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl SslFilter {
    fn with_buffers<F, R>(&self, buf: &mut FilterBuf<'_>, f: F) -> R
    where
        F: FnOnce(&mut FilterBuf<'_>) -> R,
    {
        self.inner.borrow_mut().get_mut().source = buf.read_src().take();
        let result = f(buf);
        if let Some(src) = self.inner.borrow_mut().get_mut().source.take()
            && !src.is_empty()
        {
            *buf.read_src() = Some(src);
        }
        result
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
                .is_some_and(|protos| protos.windows(2).any(|w| w == H2));
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
                    cert_chain.iter().map(ToOwned::to_owned).collect(),
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

    fn shutdown(&self, buf: &mut FilterBuf<'_>) -> io::Result<Poll<()>> {
        if self.inner.borrow().get_ref().stopped {
            return Ok(Poll::Ready(()));
        }
        let ssl_result = self.with_buffers(buf, |_| self.inner.borrow_mut().shutdown());

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
            Err(e) => Err(e.into_io_error().unwrap_or_else(io::Error::other)),
        }
    }

    fn process_read_buf(&self, rb: &mut FilterBuf<'_>) -> io::Result<()> {
        self.with_buffers(rb, |buf| {
            buf.with_read_buffers(|io, _, dst| {
                loop {
                    io.resize_read_buf(dst);

                    let chunk: &mut [u8] =
                        unsafe { &mut *(&raw mut *dst.chunk_mut() as *mut [u8]) };
                    let ssl_result = self.inner.borrow_mut().ssl_read(chunk);
                    let result = match ssl_result {
                        Ok(v) => {
                            unsafe { dst.advance_mut(v) };
                            continue;
                        }
                        Err(ref e)
                            if e.code() == ssl::ErrorCode::WANT_READ
                                || e.code() == ssl::ErrorCode::WANT_WRITE =>
                        {
                            Ok(())
                        }
                        Err(ref e) if e.code() == ssl::ErrorCode::ZERO_RETURN => {
                            self.inner.borrow_mut().get_mut().stopped = true;
                            io.want_shutdown();
                            Ok(())
                        }
                        Err(e) => {
                            log::trace!("{}: SSL Error: {:?}", io.tag(), e);
                            Err(map_to_ioerr(e))
                        }
                    };
                    return result;
                }
            })
        })
    }

    fn process_write_buf(&self, wb: &mut FilterBuf<'_>) -> io::Result<()> {
        self.with_buffers(wb, |buf| {
            buf.with_buffers(|_, _, _, w_src, w_dst| {
                let mut inner = self.inner.borrow_mut();
                while let Some(mut page) = w_src.take() {
                    match inner.ssl_write(&page) {
                        Ok(v) => {
                            page.advance_to(v);
                            w_src.prepend(page);
                        }
                        Err(e) => {
                            return match e.code() {
                                ssl::ErrorCode::WANT_READ | ssl::ErrorCode::WANT_WRITE => {
                                    Ok(())
                                }
                                _ => Err(map_to_ioerr(e)),
                            };
                        }
                    }
                }
                inner.get_mut().destination.move_to(w_dst);
                Ok(())
            })
        })
    }
}

/// Create openssl connector filter factory
pub async fn connect<F: Filter>(
    io: Io<F>,
    ssl: ssl::Ssl,
) -> Result<Io<Layer<SslFilter, F>>, io::Error> {
    let inner = IoInner {
        source: None,
        destination: BytePages::new(io.cfg().write_page_size()),
        stopped: false,
    };
    let filter = SslFilter {
        inner: RefCell::new(ssl::SslStream::new(ssl, inner)?),
    };
    let io = io.add_filter(filter);

    loop {
        let result = io.with_buf(|buf| {
            let filter = io.filter();
            filter.with_buffers(buf, |_| filter.inner.borrow_mut().connect())
        })?;
        if handle_result(&io, result).await?.is_some() {
            break;
        }
    }

    Ok(io)
}

async fn handle_result<F>(
    io: &Io<F>,
    result: Result<(), ssl::Error>,
) -> io::Result<Option<()>> {
    match result {
        Ok(v) => Ok(Some(v)),
        Err(e) => match e.code() {
            ssl::ErrorCode::WANT_READ => match io.read_notify().await? {
                None => Err(io::Error::new(io::ErrorKind::NotConnected, "disconnected")),
                _ => Ok(None),
            },
            ssl::ErrorCode::WANT_WRITE => Ok(None),
            _ => Err(io::Error::other(e)),
        },
    }
}

fn map_to_ioerr<E: Into<Box<dyn Error + Send + Sync>>>(err: E) -> io::Error {
    io::Error::other(err)
}
