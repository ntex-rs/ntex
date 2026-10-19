//! TLS server filter backed by rustls
use std::{any, cell::UnsafeCell, io, sync::Arc, task::Poll};

use ntex_io::{Filter, FilterBuf, FilterLayer, Io, Layer};
use ntex_util::time::Millis;
use tls_rustls::{ServerConfig, ServerConnection};

use crate::{Servername, rustls::Stream};

#[derive(Debug)]
/// An implementation of SSL streams
pub struct TlsServerFilter {
    session: UnsafeCell<ServerConnection>,
}

impl FilterLayer for TlsServerFilter {
    fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>> {
        self.stream(|s| {
            if id == any::TypeId::of::<Servername>() {
                let name = s.session.server_name()?;
                Some(Box::new(Servername(name.to_string())) as Box<dyn any::Any>)
            } else {
                s.query(id)
            }
        })
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

impl TlsServerFilter {
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
                session: UnsafeCell::new(session),
            });

            crate::utils::handshake(&io, || Ok(io.filter().is_handshaking())).await?;
            log::trace!("{}: TLS Handshake successed", io.tag());
            Ok(io)
        })
        .await
    }

    fn is_handshaking(&self) -> bool {
        unsafe { &*self.session.get() }.is_handshaking()
    }

    pub(super) fn stream<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut Stream<'_, ServerConnection>) -> R,
    {
        #[cfg(test)]
        let _borrow = crate::utils::testing::TlsBorrow::new();

        let mut s = Stream::new(unsafe { &mut *self.session.get() });
        f(&mut s)
    }
}
