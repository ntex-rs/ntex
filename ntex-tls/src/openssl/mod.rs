//! An implementation of SSL streams for ntex backed by OpenSSL
use std::{any, cell::RefCell, cmp, error::Error, io, task::Poll};

use ntex_bytes::{BufMut, BytesVec};
use ntex_io::{Filter, FilterLayer, Io, Layer, ReadBuf, WriteBuf, types};
use tls_openssl::ssl::{self, NameType, SslStream};
use tls_openssl::x509::X509;

use crate::{PskIdentity, Servername};

mod connect;
pub use self::connect::{SslConnector, SslConnectorService};

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
    source: Option<BytesVec>,
    destination: Option<BytesVec>,
}

impl io::Read for IoInner {
    fn read(&mut self, dst: &mut [u8]) -> io::Result<usize> {
        if let Some(ref mut buf) = self.source {
            if buf.is_empty() {
                Err(io::Error::from(io::ErrorKind::WouldBlock))
            } else {
                let len = cmp::min(buf.len(), dst.len());
                dst[..len].copy_from_slice(&buf[..len]);
                buf.advance(len);
                Ok(len)
            }
        } else {
            Err(io::Error::from(io::ErrorKind::WouldBlock))
        }
    }
}

impl io::Write for IoInner {
    fn write(&mut self, src: &[u8]) -> io::Result<usize> {
        self.destination.as_mut().unwrap().extend_from_slice(src);
        Ok(src.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl SslFilter {
    fn with_buffers<F, R>(&self, buf: &WriteBuf<'_>, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        self.inner.borrow_mut().get_mut().destination = Some(buf.take_dst());
        self.inner.borrow_mut().get_mut().source = buf.with_read_buf(|b| b.take_src());
        let result = f();
        buf.set_dst(self.inner.borrow_mut().get_mut().destination.take());
        buf.with_read_buf(|b| b.set_src(self.inner.borrow_mut().get_mut().source.take()));
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

    fn shutdown(&self, buf: &WriteBuf<'_>) -> io::Result<Poll<()>> {
        let ssl_result = self.with_buffers(buf, || self.inner.borrow_mut().shutdown());

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

    fn process_read_buf(&self, buf: &ReadBuf<'_>) -> io::Result<usize> {
        buf.with_write_buf(|b| {
            self.with_buffers(b, || {
                buf.with_dst(|dst| {
                    let mut new_bytes = 0;
                    loop {
                        buf.resize_buf(dst);

                        let chunk: &mut [u8] =
                            unsafe { std::mem::transmute(&mut *dst.chunk_mut()) };
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
                                log::debug!("{}: SSL Error: {:?}", buf.tag(), e);
                                buf.want_shutdown();
                                Ok(new_bytes)
                            }
                            Err(e) => {
                                log::trace!("{}: SSL Error: {:?}", buf.tag(), e);
                                Err(map_to_ioerr(e))
                            }
                        };
                        return result;
                    }
                })
            })
        })
    }

    fn process_write_buf(&self, wb: &WriteBuf<'_>) -> io::Result<()> {
        wb.with_src(|b| {
            if let Some(src) = b {
                self.with_buffers(wb, || {
                    loop {
                        if src.is_empty() {
                            return Ok(());
                        }
                        let ssl_result = self.inner.borrow_mut().ssl_write(src);
                        match ssl_result {
                            Ok(v) => {
                                src.advance(v);
                                continue;
                            }
                            Err(e) => {
                                return match e.code() {
                                    ssl::ErrorCode::WANT_READ
                                    | ssl::ErrorCode::WANT_WRITE => Ok(()),
                                    _ => Err(map_to_ioerr(e)),
                                };
                            }
                        }
                    }
                })
            } else {
                Ok(())
            }
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
        destination: None,
    };
    let filter = SslFilter {
        inner: RefCell::new(ssl::SslStream::new(ssl, inner)?),
    };
    let io = io.add_filter(filter);

    loop {
        let result = io.with_buf(|buf| {
            let filter = io.filter();
            filter.with_buffers(buf, || filter.inner.borrow_mut().connect())
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
            ssl::ErrorCode::WANT_READ => {
                let res = io.read_notify().await;
                match res? {
                    None => {
                        Err(io::Error::new(io::ErrorKind::NotConnected, "disconnected"))
                    }
                    _ => Ok(None),
                }
            }
            ssl::ErrorCode::WANT_WRITE => Ok(None),
            _ => Err(io::Error::other(e)),
        },
    }
}

fn map_to_ioerr<E: Into<Box<dyn Error + Send + Sync>>>(err: E) -> io::Error {
    io::Error::other(err)
}
