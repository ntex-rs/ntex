use std::task::{Context, Poll};
use std::{io, marker::PhantomData, sync::Arc};

use tls_rust::ServerConfig;

use ntex_io::{Filter, FilterFactory, Io, Layer};
use ntex_service::{Service, ServiceCtx, ServiceFactory};
use ntex_util::time::Millis;

use super::{TlsAcceptor, TlsFilter};
use crate::{counter::Counter, MAX_SSL_ACCEPT_COUNTER};

#[derive(Debug)]
/// Support `SSL` connections via rustls package
///
/// `rust-tls` feature enables `RustlsAcceptor` type
pub struct Acceptor<F> {
    inner: TlsAcceptor,
    _t: PhantomData<F>,
}

impl<F> Acceptor<F> {
    /// Create rustls based `Acceptor` service factory
    pub fn new(config: Arc<ServerConfig>) -> Self {
        Acceptor {
            inner: TlsAcceptor::new(config),
            _t: PhantomData,
        }
    }

    /// Set handshake timeout.
    ///
    /// Default is set to 5 seconds.
    pub fn timeout<U: Into<Millis>>(mut self, timeout: U) -> Self {
        self.inner.timeout(timeout.into());
        self
    }
}

impl<F> From<ServerConfig> for Acceptor<F> {
    fn from(cfg: ServerConfig) -> Self {
        Self::new(Arc::new(cfg))
    }
}

impl<F> Clone for Acceptor<F> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            _t: PhantomData,
        }
    }
}

impl<F: Filter, C: 'static> ServiceFactory<Io<F>, C> for Acceptor<F> {
    type Response = Io<Layer<TlsFilter, F>>;
    type Error = io::Error;
    type Service = AcceptorService<F>;
    type InitError = ();

    async fn create(&self, _: C) -> Result<Self::Service, Self::InitError> {
        MAX_SSL_ACCEPT_COUNTER.with(|conns| {
            Ok(AcceptorService {
                acceptor: self.inner.clone(),
                conns: conns.clone(),
                io: PhantomData,
            })
        })
    }
}

#[derive(Debug)]
/// RusTLS based `Acceptor` service
pub struct AcceptorService<F> {
    acceptor: TlsAcceptor,
    io: PhantomData<F>,
    conns: Counter,
}

impl<F: Filter> Service<Io<F>> for AcceptorService<F> {
    type Response = Io<Layer<TlsFilter, F>>;
    type Error = io::Error;

    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if self.conns.available(cx) {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }

    async fn call(
        &self,
        req: Io<F>,
        _: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        let _guard = self.conns.get();
        self.acceptor.clone().create(req).await
    }
}
