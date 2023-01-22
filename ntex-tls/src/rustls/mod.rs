#![allow(clippy::type_complexity)]
//! An implementation of SSL streams for ntex backed by OpenSSL
use std::{any, cell::Cell, cmp, io, sync::Arc, task::Context, task::Poll};

use ntex_bytes::PoolRef;
use ntex_io::{
    Buffer, Filter, FilterFactory, FilterLayer, Io, Layer, ReadStatus, WriteStatus,
};
use ntex_util::{future::BoxFuture, time::Millis};
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
pub struct TlsFilter {
    inner: InnerTlsFilter,
}

enum InnerTlsFilter {
    Server(TlsServerFilter),
    Client(TlsClientFilter),
}

impl TlsFilter {
    fn new_server(server: TlsServerFilter) -> Self {
        TlsFilter {
            inner: InnerTlsFilter::Server(server),
        }
    }
    fn new_client(client: TlsClientFilter) -> Self {
        TlsFilter {
            inner: InnerTlsFilter::Client(client),
        }
    }
    fn server(&self) -> &TlsServerFilter {
        match self.inner {
            InnerTlsFilter::Server(ref server) => server,
            _ => unreachable!(),
        }
    }
    fn client(&self) -> &TlsClientFilter {
        match self.inner {
            InnerTlsFilter::Client(ref server) => server,
            _ => unreachable!(),
        }
    }
}

impl Filter for TlsFilter {
    #[inline]
    fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>> {
        match self.inner {
            InnerTlsFilter::Server(ref f) => f.query(id),
            InnerTlsFilter::Client(ref f) => f.query(id),
        }
    }

    #[inline]
    fn shutdown(&self, buf: &mut Buffer<'_>) -> io::Result<Poll<()>> {
        match self.inner {
            InnerTlsFilter::Server(ref f) => f.shutdown(buf),
            InnerTlsFilter::Client(ref f) => f.shutdown(buf),
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
    fn process_read_buf(&self, buf: &mut Buffer<'_>, nbytes: usize) -> io::Result<usize> {
        match self.inner {
            InnerTlsFilter::Server(ref f) => f.process_read_buf(buf, nbytes),
            InnerTlsFilter::Client(ref f) => f.process_read_buf(buf, nbytes),
        }
    }

    #[inline]
    fn process_write_buf(&self, buf: &mut Buffer<'_>) -> io::Result<()> {
        match self.inner {
            InnerTlsFilter::Server(ref f) => f.process_write_buf(buf),
            InnerTlsFilter::Client(ref f) => f.process_write_buf(buf),
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

impl<F: FilterLayer> FilterFactory<F> for TlsAcceptor {
    type Filter = TlsFilter;

    type Error = io::Error;
    type Future = BoxFuture<'static, Result<Io<Layer<Self::Filter, F>>, io::Error>>;

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

impl<F: FilterLayer> FilterFactory<F> for TlsConnectorConfigured {
    type Filter = TlsFilter;

    type Error = io::Error;
    type Future = BoxFuture<'static, Result<Io<Layer<Self::Filter, F>>, io::Error>>;

    fn create(self, st: Io<F>) -> Self::Future {
        let cfg = self.cfg;
        let server_name = self.server_name;

        Box::pin(async move { TlsClientFilter::create(st, cfg, server_name).await })
    }
}

pub(crate) struct IoInner {
    pool: PoolRef,
    handshake: Cell<bool>,
}

pub(crate) struct Wrapper<'a, 'b>(&'a IoInner, &'a mut Buffer<'b>);

impl<'a, 'b> io::Read for Wrapper<'a, 'b> {
    fn read(&mut self, dst: &mut [u8]) -> io::Result<usize> {
        let read_buf = self.1.get_read_src();
        let len = cmp::min(read_buf.len(), dst.len());
        let result = if len > 0 {
            dst[..len].copy_from_slice(&read_buf.split_to(len));
            Ok(len)
        } else {
            Err(io::Error::new(io::ErrorKind::WouldBlock, ""))
        };
        result
    }
}

impl<'a, 'b> io::Write for Wrapper<'a, 'b> {
    fn write(&mut self, src: &[u8]) -> io::Result<usize> {
        let buf = self.1.get_write_dst();
        buf.reserve(src.len());
        buf.extend_from_slice(src);
        Ok(src.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
