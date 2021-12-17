#![allow(dead_code, unused_imports, clippy::type_complexity)]
//! An implementation of SSL streams for ntex backed by OpenSSL
use std::sync::Arc;
use std::{
    any, cmp, error::Error, future::Future, io, pin::Pin, task::Context, task::Poll,
};

use ntex_bytes::{BufMut, BytesMut};
use ntex_io::{
    Filter, FilterFactory, Io, IoRef, ReadFilter, WriteFilter, WriteReadiness,
};
use ntex_util::{future::Ready, time::Millis};
use tls_rust::{ClientConfig, ServerConfig, ServerName};

use super::types;

mod accept;
pub use accept::{Acceptor, AcceptorService};

/// An implementation of SSL streams
pub struct TlsFilter<F> {
    inner: F,
}

impl<F: Filter> Filter for TlsFilter<F> {
    fn shutdown(&self, st: &IoRef) -> Poll<Result<(), io::Error>> {
        self.inner.shutdown(st)
    }

    fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>> {
        self.inner.query(id)
    }
}

impl<F: Filter> ReadFilter for TlsFilter<F> {
    fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), ()>> {
        self.inner.poll_read_ready(cx)
    }

    fn read_closed(&self, err: Option<io::Error>) {
        self.inner.read_closed(err)
    }

    fn get_read_buf(&self) -> Option<BytesMut> {
        self.inner.get_read_buf()
    }

    fn release_read_buf(
        &self,
        src: BytesMut,
        new_bytes: usize,
    ) -> Result<(), io::Error> {
        self.inner.release_read_buf(src, new_bytes)
    }
}

impl<F: Filter> WriteFilter for TlsFilter<F> {
    fn poll_write_ready(
        &self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), WriteReadiness>> {
        self.inner.poll_write_ready(cx)
    }

    fn write_closed(&self, err: Option<io::Error>) {
        self.inner.read_closed(err)
    }

    fn get_write_buf(&self) -> Option<BytesMut> {
        self.inner.get_write_buf()
    }

    fn release_write_buf(&self, buf: BytesMut) -> Result<(), io::Error> {
        self.inner.release_write_buf(buf)
    }
}

pub struct TlsAcceptor {
    cfg: Arc<ServerConfig>,
    timeout: Millis,
}

impl TlsAcceptor {
    /// Create openssl acceptor filter factory
    pub fn new(cfg: ServerConfig) -> Self {
        TlsAcceptor {
            cfg: Arc::new(cfg),
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

    type Error = Box<dyn Error>;
    type Future = Ready<Io<Self::Filter>, Self::Error>;

    fn create(self, st: Io<F>) -> Self::Future {
        st.map_filter::<Self, _>(|inner: F| Ok(TlsFilter { inner }))
            .into()
    }
}

pub struct TlsConnector {
    cfg: Arc<ClientConfig>,
}

impl TlsConnector {
    /// Create openssl connector filter factory
    pub fn new(cfg: ClientConfig) -> Self {
        TlsConnector { cfg: Arc::new(cfg) }
    }
}

impl Clone for TlsConnector {
    fn clone(&self) -> Self {
        Self {
            cfg: self.cfg.clone(),
        }
    }
}

impl<F: Filter + 'static> FilterFactory<F> for TlsConnector {
    type Filter = TlsFilter<F>;

    type Error = Box<dyn Error>;
    type Future = Ready<Io<Self::Filter>, Self::Error>;

    fn create(self, st: Io<F>) -> Self::Future {
        st.map_filter::<Self, _>(|inner| Ok(TlsFilter { inner }))
            .into()
    }
}
