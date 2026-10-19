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
        let mut peer_closed = false;
        let result = self.stream(|s| s.process_read_buf(buf, &mut peer_closed));
        // Release the session before closing: the read-task waker may reenter.
        if peer_closed {
            buf.io().close();
        }
        result
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
        session.set_buffer_limit(Some(io.cfg().write_size().capacity()));
        let io = io.add_filter(TlsClientFilter {
            session: UnsafeCell::new(session),
        });

        crate::utils::handshake(&io, || Ok(io.filter().is_handshaking())).await?;
        Ok(io)
    }

    fn is_handshaking(&self) -> bool {
        unsafe { &*self.session.get() }.is_handshaking()
    }

    pub(super) fn stream<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut Stream<'_, ClientConnection>) -> R,
    {
        #[cfg(test)]
        let _borrow = crate::utils::testing::TlsBorrow::new();

        let mut s = Stream::new(unsafe { &mut *self.session.get() });
        f(&mut s)
    }
}
