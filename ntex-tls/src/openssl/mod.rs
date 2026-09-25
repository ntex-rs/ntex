//! An implementation of SSL streams for ntex backed by OpenSSL
use std::{any, borrow::ToOwned, cell::UnsafeCell, cmp, io, mem::MaybeUninit, ptr, task::Poll};

use foreign_types_shared::ForeignType;
use ntex_bytes::{BufMut, BytePages, BytesMut};
use ntex_io::{Filter, FilterBuf, FilterLayer, Io, Layer, types};
use openssl_sys as ffi;
use tls_openssl::ssl::{self, NameType, SslStream};
use tls_openssl::x509::X509;

use crate::{PskIdentity, Servername};

mod connect;
pub use self::connect::SslConnector;

mod accept;
pub use self::accept::SslAcceptor;

/// Connection's peer cert
#[derive(Debug)]
pub struct PeerCert(pub X509);

/// Connection's peer cert chain
#[derive(Debug)]
pub struct PeerCertChain(pub Vec<X509>);

/// An implementation of SSL streams
#[derive(Debug)]
pub struct SslFilter {
    inner: UnsafeCell<SslStream<IoInner>>,
}

#[derive(Debug)]
struct IoInner {
    source: Option<BytesMut>,
    destination: BytePages,
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
    fn new(stream: SslStream<IoInner>) -> Self {
        Self {
            inner: UnsafeCell::new(stream),
        }
    }

    fn ssl(&self) -> &ssl::SslRef {
        // SAFETY: the filter is single-threaded, and a mutable reference to
        // the stream exists only inside `with_buffers`, which never calls back
        // into the filter.
        unsafe { (*self.inner.get()).ssl() }
    }

    fn with_buffers<F, R>(&self, buf: &FilterBuf<'_>, f: F) -> R
    where
        F: FnOnce(&mut SslStream<IoInner>, &FilterBuf<'_>) -> R,
    {
        // SAFETY: see `ssl()`. Neither the BIO callbacks nor the buffer
        // operations below re-enter the filter.
        let stream = unsafe { &mut *self.inner.get() };

        let st = stream.get_mut();
        st.source = buf.with_read_src(Option::take);

        // get current page from destination buffer (optimization)
        buf.with_write_buffers(|_, dst| st.destination.try_get_current_from(dst));

        let result = f(stream, buf);

        let st = stream.get_mut();
        if let Some(src) = st.source.take()
            && !src.is_empty()
        {
            buf.with_read_src(|buf| *buf = Some(src));
        }

        // copy internal buffer to write dst buffer
        if !st.destination.is_empty() {
            buf.with_write_buffers(|_, dst| {
                st.destination.move_to(dst);
            });
        }
        result
    }
}

impl FilterLayer for SslFilter {
    fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>> {
        const H2: &[u8] = b"h2";

        if id == any::TypeId::of::<types::HttpProtocol>() {
            let h2 = self
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
            if let Some(cert) = self.ssl().peer_certificate() {
                Some(Box::new(PeerCert(cert)))
            } else {
                None
            }
        } else if id == any::TypeId::of::<PeerCertChain>() {
            if let Some(cert_chain) = self.ssl().peer_cert_chain() {
                Some(Box::new(PeerCertChain(
                    cert_chain.iter().map(ToOwned::to_owned).collect(),
                )))
            } else {
                None
            }
        } else if id == any::TypeId::of::<Servername>() {
            if let Some(name) = self.ssl().servername(NameType::HOST_NAME) {
                Some(Box::new(Servername(name.to_string())))
            } else {
                None
            }
        } else if id == any::TypeId::of::<PskIdentity>() {
            if let Some(psk_id) = self.ssl().psk_identity() {
                Some(Box::new(PskIdentity(psk_id.to_vec())))
            } else {
                None
            }
        } else {
            None
        }
    }

    fn shutdown(&self, buf: &FilterBuf<'_>) -> io::Result<Poll<()>> {
        let ssl_result = self.with_buffers(buf, |s, _| s.shutdown());
        let result = match ssl_result {
            Ok(ssl::ShutdownResult::Sent) => Ok(Poll::Pending),
            Ok(ssl::ShutdownResult::Received) => Ok(Poll::Ready(())),
            Err(ref e) if e.code() == ssl::ErrorCode::ZERO_RETURN => Ok(Poll::Ready(())),
            Err(ref e)
                if matches!(
                    e.code(),
                    ssl::ErrorCode::WANT_READ | ssl::ErrorCode::WANT_WRITE
                ) =>
            {
                Ok(Poll::Pending)
            }
            Err(e) => Err(e.into_io_error().unwrap_or_else(io::Error::other)),
        };

        // Our close_notify has been sent, but the peer closed the connection
        // without sending its own; it is never going to arrive.
        if matches!(result, Ok(Poll::Pending)) && buf.io().is_read_eof() {
            return Ok(Poll::Ready(()));
        }
        result
    }

    fn process_read_buf(&self, rb: &FilterBuf<'_>) -> io::Result<()> {
        self.with_buffers(rb, |stream, buf| {
            buf.with_read_buffers(|_, dst| {
                loop {
                    if dst.remaining_mut() == 0 {
                        rb.io().resize_read_buf(dst);
                    }

                    let chunk = dst.chunk_mut();
                    let chunk = unsafe {
                        std::slice::from_raw_parts_mut(
                            chunk.as_mut_ptr().cast::<MaybeUninit<u8>>(),
                            chunk.len(),
                        )
                    };
                    let result = match stream.ssl_read_uninit(chunk) {
                        Ok(v) => {
                            unsafe { dst.advance_mut(v) };
                            continue;
                        }
                        Err(ref e) if e.code() == ssl::ErrorCode::WANT_READ => Ok(()),
                        Err(ref e) if e.code() == ssl::ErrorCode::WANT_WRITE => Ok(()),
                        Err(ref e) if e.code() == ssl::ErrorCode::ZERO_RETURN => {
                            rb.io().close();
                            Ok(())
                        }
                        Err(e) => {
                            log::trace!("{}: SSL Error: {:?}", rb.tag(), e);
                            Err(io::Error::other(e))
                        }
                    };
                    return result;
                }
            })
        })
    }

    fn process_write_buf(&self, wb: &FilterBuf<'_>) -> io::Result<()> {
        self.with_buffers(wb, |stream, buf| {
            buf.with_write_buffers(|w_src, _| {
                if !w_src.is_empty() {
                    while let Some(mut page) = w_src.take() {
                        match stream.ssl_write(&page) {
                            Ok(v) => {
                                page.advance_to(v);
                                w_src.prepend(page);
                            }
                            Err(e)
                                if matches!(
                                    e.code(),
                                    ssl::ErrorCode::WANT_READ | ssl::ErrorCode::WANT_WRITE
                                ) =>
                            {
                                break;
                            }
                            Err(e) => return Err(io::Error::other(e)),
                        }
                    }
                }
                Ok(())
            })
        })
    }
}

fn new_stream<F>(io: &Io<F>, ssl: ssl::Ssl) -> io::Result<SslStream<IoInner>> {
    // Let OpenSSL pull all buffered ciphertext in one BIO read, instead of
    // reading every record header and body separately.
    unsafe {
        ffi::SSL_ctrl(
            ssl.as_ptr(),
            ffi::SSL_CTRL_SET_READ_AHEAD,
            1,
            ptr::null_mut(),
        );
    }

    let inner = IoInner {
        source: None,
        destination: BytePages::new(io.cfg().write_page_size()),
    };
    Ok(SslStream::new(ssl, inner)?)
}

/// Create openssl connector filter factory
pub async fn connect<F: Filter>(
    io: Io<F>,
    ssl: ssl::Ssl,
) -> Result<Io<Layer<SslFilter, F>>, io::Error> {
    let mut stream = new_stream(&io, ssl)?;
    let _ = stream.connect();

    let filter = SslFilter::new(stream);
    let io = io.add_filter(filter);

    loop {
        let result = io.with_buf(|buf| {
            let filter = io.filter();
            filter.with_buffers(buf, |s, _| s.connect())
        })?;

        if handle_result(&io, result).await?.is_some() {
            break;
        }
    }

    Ok(io)
}

async fn handle_result<F>(io: &Io<F>, result: Result<(), ssl::Error>) -> io::Result<Option<()>> {
    match result {
        Ok(v) => Ok(Some(v)),
        Err(e) => match e.code() {
            ssl::ErrorCode::WANT_READ => match io.read_notify().await? {
                None => Err(io::Error::new(io::ErrorKind::UnexpectedEof, "disconnected")),
                _ => Ok(None),
            },
            ssl::ErrorCode::WANT_WRITE => Ok(None),
            _ => Err(io::Error::other(e)),
        },
    }
}
