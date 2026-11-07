//! Server-side rustls session ownership and filter integration.
use std::{any, cell::RefCell, io, sync::Arc, task::Poll};

use ntex_io::{Filter, FilterBuf, FilterLayer, Io, Layer};
use ntex_util::time::Millis;
use tls_rustls::{ServerConfig, ServerConnection};

use crate::{Servername, rustls::Stream};

#[derive(Debug)]
/// Encrypts and decrypts a server connection using rustls.
///
/// Construct through [`Self::create`] or [`super::TlsAcceptor`]. In addition to
/// the shared TLS metadata, the filter exposes the client's SNI as
/// [`Servername`]. Shutdown exchanges `close_notify`, accepting transport EOF
/// when the peer has already disconnected.
pub struct TlsServerFilter {
    /// Single-threaded session; mutable access is scoped to `stream` calls.
    session: RefCell<ServerConnection>,
}

impl FilterLayer for TlsServerFilter {
    /// Returns `None` while the session is busy, for example when a
    /// certificate resolver queries its own connection during the handshake.
    fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>> {
        let mut session = self.session.try_borrow_mut().ok()?;
        if id == any::TypeId::of::<Servername>() {
            let name = session.server_name()?;
            Some(Box::new(Servername(name.to_string())))
        } else {
            Stream::new(&mut *session).query(id)
        }
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

impl TlsServerFilter {
    /// Installs a server filter and completes the handshake within `timeout`.
    ///
    /// Zero disables the deadline. The timeout is supplied directly, not read
    /// from [`crate::TlsConfig`], and this constructor does not acquire an
    /// acceptor-capacity slot. Existing filters remain below TLS.
    ///
    /// The session's buffering limit is initialized from the I/O write-page
    /// capacity; certificates and client authentication follow `cfg`.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::TimedOut`] on expiration, or an error from
    /// session creation, negotiation, or transport I/O. Premature transport EOF
    /// returns [`io::ErrorKind::UnexpectedEof`].
    pub async fn create<F: Filter>(
        io: Io<F>,
        cfg: Arc<ServerConfig>,
        timeout: Millis,
    ) -> Result<Io<Layer<TlsServerFilter, F>>, io::Error> {
        log::trace!("{}: Initiate server connection", io.tag());

        crate::utils::with_timeout(timeout, async {
            let mut session = ServerConnection::new(cfg).map_err(io::Error::other)?;
            session.set_buffer_limit(Some(io.cfg().write_size().capacity()));
            let io = io.add_filter(TlsServerFilter {
                session: RefCell::new(session),
            });

            crate::utils::handshake(&io, || Ok(io.filter().is_handshaking())).await?;
            log::trace!("{}: TLS Handshake successed", io.tag());
            Ok(io)
        })
        .await
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
        F: FnOnce(&mut Stream<'_, ServerConnection>) -> R,
    {
        #[cfg(test)]
        let _borrow = crate::utils::testing::TlsBorrow::new();

        let mut session = self.session.borrow_mut();
        f(&mut Stream::new(&mut *session))
    }
}
