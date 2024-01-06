use std::task::{Context, Poll};
use std::{error::Error, marker::PhantomData};

use ntex_io::{Filter, FilterFactory, Io, Layer};
use ntex_service::{Service, ServiceCtx, ServiceFactory};
use ntex_util::time::Millis;
use tls_openssl::ssl::SslAcceptor;

use crate::counter::Counter;
use crate::MAX_SSL_ACCEPT_COUNTER;

use super::{SslAcceptor as IoSslAcceptor, SslFilter};

#[derive(Debug)]
/// Support `TLS` server connections via openssl package
///
/// `openssl` feature enables `Acceptor` type
pub struct Acceptor<F> {
    acceptor: IoSslAcceptor,
    _t: PhantomData<F>,
}

impl<F> Acceptor<F> {
    /// Create default openssl acceptor service
    pub fn new(acceptor: SslAcceptor) -> Self {
        Acceptor {
            acceptor: IoSslAcceptor::new(acceptor),
            _t: PhantomData,
        }
    }

    /// Set handshake timeout.
    ///
    /// Default is set to 5 seconds.
    pub fn timeout<U: Into<Millis>>(mut self, timeout: U) -> Self {
        self.acceptor.timeout(timeout);
        self
    }
}

impl<F> From<SslAcceptor> for Acceptor<F> {
    fn from(acceptor: SslAcceptor) -> Self {
        Self::new(acceptor)
    }
}

impl<F> Clone for Acceptor<F> {
    fn clone(&self) -> Self {
        Self {
            acceptor: self.acceptor.clone(),
            _t: PhantomData,
        }
    }
}

impl<F: Filter, C: 'static> ServiceFactory<Io<F>, C> for Acceptor<F> {
    type Response = Io<Layer<SslFilter, F>>;
    type Error = Box<dyn Error>;
    type Service = AcceptorService<F>;
    type InitError = ();

    async fn create(&self, _: C) -> Result<Self::Service, Self::InitError> {
        MAX_SSL_ACCEPT_COUNTER.with(|conns| {
            Ok(AcceptorService {
                acceptor: self.acceptor.clone(),
                conns: conns.clone(),
                _t: PhantomData,
            })
        })
    }
}

#[derive(Debug)]
/// Support `TLS` server connections via openssl package
///
/// `openssl` feature enables `Acceptor` type
pub struct AcceptorService<F> {
    acceptor: IoSslAcceptor,
    conns: Counter,
    _t: PhantomData<F>,
}

impl<F: Filter> Service<Io<F>> for AcceptorService<F> {
    type Response = Io<Layer<SslFilter, F>>;
    type Error = Box<dyn Error>;

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
