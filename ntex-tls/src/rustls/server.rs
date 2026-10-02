//! TLS server filter backed by rustls
use std::{any, cell::UnsafeCell, fmt, io, sync::Arc, task::Poll};

use ntex_io::{Filter, FilterBuf, FilterLayer, Io, Layer};
use ntex_util::time::Millis;
use tls_rustls::{ServerConfig, server::UnbufferedServerConnection};

use super::stream::{ServerSession, Stream};
use crate::Servername;

/// An implementation of SSL streams
pub struct TlsServerFilter {
    inner: UnsafeCell<Stream<ServerSession>>,
}

impl FilterLayer for TlsServerFilter {
    fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>> {
        self.stream(|s| {
            s.query(id).or_else(|| {
                if id == any::TypeId::of::<Servername>() {
                    s.session
                        .server_name()
                        .map(|name| Box::new(Servername(name.to_string())) as Box<dyn any::Any>)
                } else {
                    None
                }
            })
        })
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

impl TlsServerFilter {
    pub async fn create<F: Filter>(
        io: Io<F>,
        cfg: Arc<ServerConfig>,
        timeout: Millis,
    ) -> Result<Io<Layer<TlsServerFilter, F>>, io::Error> {
        log::trace!("{}: Initiate server connection", io.tag());

        super::with_timeout(timeout, async {
            let session = UnbufferedServerConnection::new(cfg).map_err(io::Error::other)?;
            let io = io.add_filter(TlsServerFilter {
                inner: UnsafeCell::new(Stream::new(ServerSession::new(session))),
            });

            super::handshake(&io, || io.filter().stream(|s| s.is_handshaking())).await?;
            log::trace!("{}: TLS Handshake successed", io.tag());
            Ok(io)
        })
        .await
    }

    fn stream<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut Stream<ServerSession>) -> R,
    {
        f(unsafe { &mut *self.inner.get() })
    }
}

impl fmt::Debug for TlsServerFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TlsServerFilter").finish_non_exhaustive()
    }
}
