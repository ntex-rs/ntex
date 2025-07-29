//! An implementation of SSL streams for ntex backed by OpenSSL
use std::{any, cell::RefCell, io, sync::Arc};

use ntex_io::{Filter, FilterLayer, Io, Layer, ReadBuf, WriteBuf};
use ntex_util::{time, time::Millis};
use tls_rust::{ServerConfig, ServerConnection};

use crate::{rustls::Stream, Servername};

#[derive(Debug)]
/// An implementation of SSL streams
pub struct TlsServerFilter {
    session: RefCell<ServerConnection>,
}

impl FilterLayer for TlsServerFilter {
    fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>> {
        let session = &mut *self.session.borrow_mut();
        if let Some(item) = Stream::new(session).query(id) {
            Some(item)
        } else if id == any::TypeId::of::<Servername>() {
            if let Some(name) = session.server_name() {
                Some(Box::new(Servername(name.to_string())))
            } else {
                None
            }
        } else {
            None
        }
    }

    fn process_read_buf(&self, buf: &ReadBuf<'_>) -> io::Result<usize> {
        Stream::new(&mut *self.session.borrow_mut()).process_read_buf(buf)
    }

    fn process_write_buf(&self, buf: &WriteBuf<'_>) -> io::Result<()> {
        Stream::new(&mut *self.session.borrow_mut()).process_write_buf(buf)
    }
}

impl TlsServerFilter {
    pub async fn create<F: Filter>(
        io: Io<F>,
        cfg: Arc<ServerConfig>,
        timeout: Millis,
    ) -> Result<Io<Layer<TlsServerFilter, F>>, io::Error> {
        time::timeout(timeout, async {
            let session = ServerConnection::new(cfg).map_err(io::Error::other)?;
            let io = io.add_filter(TlsServerFilter {
                session: RefCell::new(session),
            });

            super::stream::handshake(&io.filter().session, &io).await?;
            Ok(io)
        })
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "rustls handshake timeout"))
        .and_then(|item| item)
    }
}
