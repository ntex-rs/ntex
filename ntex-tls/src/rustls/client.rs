//! Client-side rustls session ownership and filter integration.
use std::{any, cell::RefCell, io, sync::Arc, task::Poll};

use ntex_io::{Filter, FilterBuf, FilterLayer, Io, Layer};
use tls_rustls::{ClientConfig, ClientConnection, pki_types::ServerName};

use super::stream::Stream;

#[derive(Debug)]
/// Encrypts and decrypts a client connection using rustls.
///
/// Construct through [`Self::create`] or [`super::TlsConnector`]. The filter
/// exposes the metadata described in the [`super`] module and exchanges
/// `close_notify` during shutdown, accepting transport EOF if the peer has
/// already disconnected.
pub struct TlsClientFilter {
    /// Single-threaded session; mutable access is scoped to `stream` calls.
    session: RefCell<ClientConnection>,
}

impl FilterLayer for TlsClientFilter {
    /// Returns `None` while the session is busy, for example when a
    /// certificate verifier queries its own connection during the handshake.
    fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>> {
        let mut session = self.session.try_borrow_mut().ok()?;
        Stream::new(&mut *session).query(id)
    }

    /// Decrypts input, releasing the session borrow before a peer-close notification can reenter.
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
    /// Installs a client filter and drives its handshake to completion.
    ///
    /// `domain` supplies the name used by rustls verification and, for DNS names,
    /// SNI according to `cfg`. Existing filters are preserved. The session's
    /// buffering limit is initialized from the I/O write-page capacity.
    ///
    /// No handshake timeout or concurrency limit is applied here. Use
    /// [`super::TlsConnector`] for the service-configured timeout.
    ///
    /// # Errors
    ///
    /// Returns an error if session creation, certificate verification, protocol
    /// negotiation, or transport I/O fails. An incomplete handshake followed by
    /// transport EOF returns [`io::ErrorKind::UnexpectedEof`].
    pub async fn create<F: Filter>(
        io: Io<F>,
        cfg: Arc<ClientConfig>,
        domain: ServerName<'static>,
    ) -> Result<Io<Layer<TlsClientFilter, F>>, io::Error> {
        let mut session = ClientConnection::new(cfg, domain).map_err(io::Error::other)?;
        session.set_buffer_limit(Some(io.cfg().write_size().capacity()));
        let io = io.add_filter(TlsClientFilter {
            session: RefCell::new(session),
        });

        crate::utils::handshake(&io, || Ok(io.filter().is_handshaking())).await?;
        Ok(io)
    }

    /// Observes handshake completion outside any mutable session borrow.
    fn is_handshaking(&self) -> bool {
        self.session.borrow().is_handshaking()
    }

    /// Gives `f` exclusive access to the session through the shared stream adapter.
    ///
    /// The callback must not reenter this filter or trigger a notification that
    /// does so while the session is borrowed; reentrant I/O operations panic.
    pub(super) fn stream<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut Stream<'_, ClientConnection>) -> R,
    {
        #[cfg(test)]
        let _borrow = crate::utils::testing::TlsBorrow::new();

        let mut session = self.session.borrow_mut();
        f(&mut Stream::new(&mut *session))
    }
}
