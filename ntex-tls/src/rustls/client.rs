//! An implementation of SSL streams for ntex backed by OpenSSL
use std::{any, cell::RefCell, io, sync::Arc};

use ntex_io::{Filter, FilterBuf, FilterLayer, Io, Layer};
use tls_rustls::{ClientConfig, ClientConnection, pki_types::ServerName};

use super::stream::{self, Stream};

#[derive(Debug)]
/// An implementation of SSL streams
pub struct TlsClientFilter {
    session: RefCell<ClientConnection>,
}

impl FilterLayer for TlsClientFilter {
    fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>> {
        Stream::new(&mut *self.session.borrow_mut()).query(id)
    }

    fn process_read_buf(&self, buf: &mut FilterBuf<'_>) -> io::Result<()> {
        Stream::new(&mut *self.session.borrow_mut()).process_read_buf(buf)
    }

    fn process_write_buf(&self, buf: &mut FilterBuf<'_>) -> io::Result<()> {
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

        stream::handshake(&io.filter().session, &io).await?;
        Ok(io)
    }
}
