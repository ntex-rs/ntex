//! TLS client filter backed by rustls
use std::{any, cell::UnsafeCell, io, sync::Arc, task::Poll};

use ntex_io::{Filter, FilterBuf, FilterLayer, Io, Layer};
use tls_rustls::{ClientConfig, ClientConnection, pki_types::ServerName};

use super::stream::Stream;

#[derive(Debug)]
/// An implementation of TLS streams
pub struct TlsClientFilter {
    session: UnsafeCell<ClientConnection>,
}

impl FilterLayer for TlsClientFilter {
    fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>> {
        self.stream(|s| s.query(id))
    }

    fn process_read_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
        self.stream(|s| s.process_read_buf(buf))
    }

    fn process_write_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
        self.stream(|s| s.process_write_buf(buf))
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
        let mut session = ClientConnection::new(cfg, domain).map_err(io::Error::other)?;
        session.set_buffer_limit(Some(io.cfg().write_page_size().capacity()));
        let io = io.add_filter(TlsClientFilter {
            session: UnsafeCell::new(session),
        });

        super::handshake(&io, || io.filter().state()).await?;
        Ok(io)
    }

    fn state(&self) -> (bool, bool) {
        let s = unsafe { &*self.session.get() };
        (s.wants_write(), s.is_handshaking())
    }

    fn stream<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut Stream<'_, ClientConnection>) -> R,
    {
        let mut s = Stream::new(unsafe { &mut *self.session.get() });
        f(&mut s)
    }
}
