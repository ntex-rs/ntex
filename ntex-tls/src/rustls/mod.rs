#![allow(clippy::type_complexity)]
//! An implementation of SSL streams for ntex backed by OpenSSL
use std::sync::Arc;
use std::{any, future::Future, io, pin::Pin, task::Context, task::Poll};

use ntex_bytes::BytesMut;
use ntex_io::{Filter, FilterFactory, Io, IoRef, WriteReadiness};
use ntex_util::time::Millis;
use tls_rust::{ClientConfig, ServerConfig, ServerName};

mod accept;
mod client;
mod server;
pub use accept::{Acceptor, AcceptorService};

use self::client::TlsClientFilter;
use self::server::TlsServerFilter;

/// An implementation of SSL streams
pub struct TlsFilter<F> {
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
    fn shutdown(&self, st: &IoRef) -> Poll<Result<(), io::Error>> {
        match self.inner {
            InnerTlsFilter::Server(ref f) => f.shutdown(st),
            InnerTlsFilter::Client(ref f) => f.shutdown(st),
        }
    }

    #[inline]
    fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>> {
        match self.inner {
            InnerTlsFilter::Server(ref f) => f.query(id),
            InnerTlsFilter::Client(ref f) => f.query(id),
        }
    }

    #[inline]
    fn closed(&self, err: Option<io::Error>) {
        match self.inner {
            InnerTlsFilter::Server(ref f) => f.closed(err),
            InnerTlsFilter::Client(ref f) => f.closed(err),
        }
    }

    #[inline]
    fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), ()>> {
        match self.inner {
            InnerTlsFilter::Server(ref f) => f.poll_read_ready(cx),
            InnerTlsFilter::Client(ref f) => f.poll_read_ready(cx),
        }
    }

    #[inline]
    fn poll_write_ready(
        &self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), WriteReadiness>> {
        match self.inner {
            InnerTlsFilter::Server(ref f) => f.poll_write_ready(cx),
            InnerTlsFilter::Client(ref f) => f.poll_write_ready(cx),
        }
    }

    #[inline]
    fn get_read_buf(&self) -> Option<BytesMut> {
        match self.inner {
            InnerTlsFilter::Server(ref f) => f.get_read_buf(),
            InnerTlsFilter::Client(ref f) => f.get_read_buf(),
        }
    }

    #[inline]
    fn get_write_buf(&self) -> Option<BytesMut> {
        match self.inner {
            InnerTlsFilter::Server(ref f) => f.get_write_buf(),
            InnerTlsFilter::Client(ref f) => f.get_write_buf(),
        }
    }

    #[inline]
    fn release_read_buf(&self, src: BytesMut, nb: usize) -> Result<(), io::Error> {
        match self.inner {
            InnerTlsFilter::Server(ref f) => f.release_read_buf(src, nb),
            InnerTlsFilter::Client(ref f) => f.release_read_buf(src, nb),
        }
    }

    #[inline]
    fn release_write_buf(&self, src: BytesMut) -> Result<(), io::Error> {
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

impl<F: Filter + 'static> FilterFactory<F> for TlsAcceptor {
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

impl<F: Filter + 'static> FilterFactory<F> for TlsConnectorConfigured {
    type Filter = TlsFilter<F>;

    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Io<Self::Filter>, io::Error>>>>;

    fn create(self, st: Io<F>) -> Self::Future {
        let cfg = self.cfg;
        let server_name = self.server_name;

        Box::pin(async move { TlsClientFilter::create(st, cfg, server_name).await })
    }
}
