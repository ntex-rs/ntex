//! An implementation of SSL streams for ntex backed by OpenSSL
use std::{any, cell::RefCell, io, sync::Arc};

use ntex_io::{Filter, FilterLayer, Io, Layer, ReadBuf, WriteBuf};
use tls_rust::{pki_types::ServerName, ClientConfig, ClientConnection};

use super::Stream;

#[derive(Debug)]
/// An implementation of SSL streams
pub struct TlsClientFilter {
    session: RefCell<ClientConnection>,
}

impl FilterLayer for TlsClientFilter {
    fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>> {
        Stream::new(&mut *self.session.borrow_mut()).query(id)
    }

    fn process_read_buf(&self, buf: &ReadBuf<'_>) -> io::Result<usize> {
        Stream::new(&mut *self.session.borrow_mut()).process_read_buf(buf)
    }

    fn process_write_buf(&self, buf: &WriteBuf<'_>) -> io::Result<()> {
        Stream::new(&mut *self.session.borrow_mut()).process_write_buf(buf)
    }
}

impl TlsClientFilter {
    pub async fn create<F: Filter>(
        io: Io<F>,
        cfg: Arc<ClientConfig>,
        domain: ServerName<'static>,
    ) -> Result<Io<Layer<TlsClientFilter, F>>, io::Error> {
        let session = ClientConnection::new(cfg, domain).map_err(io::Error::other)?;
        let io = io.add_filter(TlsClientFilter {
            session: RefCell::new(session),
        });
        super::stream::handshake(&io.filter().session, &io).await?;
        Ok(io)
    }
}
