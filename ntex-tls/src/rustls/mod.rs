#![allow(clippy::type_complexity)]
//! An implementation of SSL streams for ntex backed by OpenSSL
use std::{any, cmp, future::Future, io, pin::Pin, task::Context, task::Poll};
use std::{cell::Cell, sync::Arc};

use ntex_bytes::{BytesVec, PoolRef};
use ntex_io::{Base, Filter, FilterFactory, Io, IoRef, ReadStatus, WriteStatus};
use ntex_util::time::Millis;
use tls_rust::{Certificate, ClientConfig, ServerConfig, ServerName};

mod accept;
mod client;
mod server;
pub use accept::{Acceptor, AcceptorService};

use self::client::TlsClientFilter;
use self::server::TlsServerFilter;

/// Connection's peer cert
#[derive(Debug)]
pub struct PeerCert(pub Certificate);

/// Connection's peer cert chain
#[derive(Debug)]
pub struct PeerCertChain(pub Vec<Certificate>);

/// An implementation of SSL streams
pub struct TlsFilter<F = Base> {
    inner: InnerTlsFilter<F>,
}

enum InnerTlsFilter<F> {
    Server(TlsServerFilter<F>),
    Client(TlsClientFilter<F>),
}

impl<F> TlsFilter<F> {
    fn new_server(server: TlsServerFilter<F>) -> Self {
        TlsFilter {
            inner: InnerTlsFilter::Server(server),
        }
    }
    fn new_client(client: TlsClientFilter<F>) -> Self {
        TlsFilter {
            inner: InnerTlsFilter::Client(client),
        }
    }
    fn server(&self) -> &TlsServerFilter<F> {
        match self.inner {
            InnerTlsFilter::Server(ref server) => server,
            _ => unreachable!(),
        }
    }
    fn client(&self) -> &TlsClientFilter<F> {
        match self.inner {
            InnerTlsFilter::Client(ref server) => server,
            _ => unreachable!(),
        }
    }
}

impl<F: Filter> Filter for TlsFilter<F> {
    #[inline]
    fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>> {
        match self.inner {
            InnerTlsFilter::Server(ref f) => f.query(id),
            InnerTlsFilter::Client(ref f) => f.query(id),
        }
    }

    #[inline]
    fn poll_shutdown(&self) -> Poll<io::Result<()>> {
        match self.inner {
            InnerTlsFilter::Server(ref f) => f.poll_shutdown(),
            InnerTlsFilter::Client(ref f) => f.poll_shutdown(),
        }
    }

    #[inline]
    fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<ReadStatus> {
        match self.inner {
            InnerTlsFilter::Server(ref f) => f.poll_read_ready(cx),
            InnerTlsFilter::Client(ref f) => f.poll_read_ready(cx),
        }
    }

    #[inline]
    fn poll_write_ready(&self, cx: &mut Context<'_>) -> Poll<WriteStatus> {
        match self.inner {
            InnerTlsFilter::Server(ref f) => f.poll_write_ready(cx),
            InnerTlsFilter::Client(ref f) => f.poll_write_ready(cx),
        }
    }

    #[inline]
    fn get_read_buf(&self) -> Option<BytesVec> {
        match self.inner {
            InnerTlsFilter::Server(ref f) => f.get_read_buf(),
            InnerTlsFilter::Client(ref f) => f.get_read_buf(),
        }
    }

    #[inline]
    fn get_write_buf(&self) -> Option<BytesVec> {
        match self.inner {
            InnerTlsFilter::Server(ref f) => f.get_write_buf(),
            InnerTlsFilter::Client(ref f) => f.get_write_buf(),
        }
    }

    #[inline]
    fn release_read_buf(&self, buf: BytesVec) {
        match self.inner {
            InnerTlsFilter::Server(ref f) => f.release_read_buf(buf),
            InnerTlsFilter::Client(ref f) => f.release_read_buf(buf),
        }
    }

    #[inline]
    fn process_read_buf(&self, io: &IoRef, nb: usize) -> io::Result<(usize, usize)> {
        match self.inner {
            InnerTlsFilter::Server(ref f) => f.process_read_buf(io, nb),
            InnerTlsFilter::Client(ref f) => f.process_read_buf(io, nb),
        }
    }

    #[inline]
    fn release_write_buf(&self, src: BytesVec) -> Result<(), io::Error> {
        match self.inner {
            InnerTlsFilter::Server(ref f) => f.release_write_buf(src),
            InnerTlsFilter::Client(ref f) => f.release_write_buf(src),
        }
    }
}

pub struct TlsAcceptor {
    cfg: Arc<ServerConfig>,
    timeout: Millis,
}

impl TlsAcceptor {
    /// Create openssl acceptor filter factory
    pub fn new(cfg: Arc<ServerConfig>) -> Self {
        TlsAcceptor {
            cfg,
            timeout: Millis(5_000),
        }
    }

    /// Set handshake timeout.
    ///
    /// Default is set to 5 seconds.
    pub fn timeout<U: Into<Millis>>(&mut self, timeout: U) -> &mut Self {
        self.timeout = timeout.into();
        self
    }
}

impl From<ServerConfig> for TlsAcceptor {
    fn from(cfg: ServerConfig) -> Self {
        Self::new(Arc::new(cfg))
    }
}

impl Clone for TlsAcceptor {
    fn clone(&self) -> Self {
        Self {
            cfg: self.cfg.clone(),
            timeout: self.timeout,
        }
    }
}

impl<F: Filter> FilterFactory<F> for TlsAcceptor {
    type Filter = TlsFilter<F>;

    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Io<Self::Filter>, io::Error>>>>;

    fn create(self, st: Io<F>) -> Self::Future {
        let cfg = self.cfg.clone();
        let timeout = self.timeout;

        Box::pin(async move { TlsServerFilter::create(st, cfg, timeout).await })
    }
}

pub struct TlsConnector {
    cfg: Arc<ClientConfig>,
}

impl TlsConnector {
    /// Create openssl connector filter factory
    pub fn new(cfg: Arc<ClientConfig>) -> Self {
        TlsConnector { cfg }
    }

    /// Set server name
    pub fn server_name(self, server_name: ServerName) -> TlsConnectorConfigured {
        TlsConnectorConfigured {
            server_name,
            cfg: self.cfg,
        }
    }
}

impl Clone for TlsConnector {
    fn clone(&self) -> Self {
        Self {
            cfg: self.cfg.clone(),
        }
    }
}

pub struct TlsConnectorConfigured {
    cfg: Arc<ClientConfig>,
    server_name: ServerName,
}

impl Clone for TlsConnectorConfigured {
    fn clone(&self) -> Self {
        Self {
            cfg: self.cfg.clone(),
            server_name: self.server_name.clone(),
        }
    }
}

impl<F: Filter> FilterFactory<F> for TlsConnectorConfigured {
    type Filter = TlsFilter<F>;

    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Io<Self::Filter>, io::Error>>>>;

    fn create(self, st: Io<F>) -> Self::Future {
        let cfg = self.cfg;
        let server_name = self.server_name;

        Box::pin(async move { TlsClientFilter::create(st, cfg, server_name).await })
    }
}

pub(crate) struct IoInner<F> {
    filter: F,
    pool: PoolRef,
    read_buf: Cell<Option<BytesVec>>,
    write_buf: Cell<Option<BytesVec>>,
    handshake: Cell<bool>,
}

pub(crate) struct Wrapper<'a, F>(&'a IoInner<F>);

impl<'a, F: Filter> io::Read for Wrapper<'a, F> {
    fn read(&mut self, dst: &mut [u8]) -> io::Result<usize> {
        if let Some(mut read_buf) = self.0.filter.get_read_buf() {
            let len = cmp::min(read_buf.len(), dst.len());
            let result = if len > 0 {
                dst[..len].copy_from_slice(&read_buf.split_to(len));
                Ok(len)
            } else {
                Err(io::Error::new(io::ErrorKind::WouldBlock, ""))
            };
            self.0.filter.release_read_buf(read_buf);
            result
        } else {
            Err(io::Error::new(io::ErrorKind::WouldBlock, ""))
        }
    }
}

impl<'a, F: Filter> io::Write for Wrapper<'a, F> {
    fn write(&mut self, src: &[u8]) -> io::Result<usize> {
        let mut buf = if let Some(mut buf) = self.0.filter.get_write_buf() {
            buf.reserve(src.len());
            buf
        } else {
            BytesVec::with_capacity_in(src.len(), self.0.pool)
        };
        buf.extend_from_slice(src);
        self.0.filter.release_write_buf(buf)?;
        Ok(src.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
