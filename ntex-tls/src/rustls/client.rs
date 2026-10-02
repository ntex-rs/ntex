//! TLS client filter backed by rustls
use std::{any, cell::UnsafeCell, fmt, io, sync::Arc, task::Poll};

use ntex_io::{Filter, FilterBuf, FilterLayer, Io, Layer};
use tls_rustls::client::UnbufferedClientConnection;
use tls_rustls::{ClientConfig, pki_types::ServerName};

use super::stream::Stream;

/// An implementation of TLS streams
pub struct TlsClientFilter {
    inner: UnsafeCell<Stream<UnbufferedClientConnection>>,
}

impl FilterLayer for TlsClientFilter {
    fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>> {
        self.stream(|s| s.query(id))
    }

    fn process_read_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
        self.stream(|s| s.process(buf))
    }

    fn process_write_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
        self.stream(|s| s.process(buf))
    }

    fn shutdown(&self, buf: &FilterBuf<'_>) -> io::Result<Poll<()>> {
        self.stream(|s| s.shutdown(buf))
    }
}

impl TlsClientFilter {
    pub async fn create<F: Filter>(
        io: Io<F>,
        cfg: Arc<ClientConfig>,
        domain: ServerName<'static>,
    ) -> Result<Io<Layer<TlsClientFilter, F>>, io::Error> {
        let session = UnbufferedClientConnection::new(cfg, domain).map_err(io::Error::other)?;
        let io = io.add_filter(TlsClientFilter {
            inner: UnsafeCell::new(Stream::new(session)),
        });

        super::handshake(&io, || io.filter().stream(|s| s.is_handshaking())).await?;
        Ok(io)
    }

    fn stream<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut Stream<UnbufferedClientConnection>) -> R,
    {
        f(unsafe { &mut *self.inner.get() })
    }
}

impl fmt::Debug for TlsClientFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TlsClientFilter").finish_non_exhaustive()
    }
}
