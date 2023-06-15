use std::task::{Context, Poll};
use std::{error::Error, future::Future, marker::PhantomData, pin::Pin};

use ntex_io::{Filter, FilterFactory, Io, Layer};
use ntex_service::{Ctx, Service, ServiceFactory};
use ntex_util::{future::Ready, time::Millis};
use tls_openssl::ssl::SslAcceptor;

use crate::counter::{Counter, CounterGuard};
use crate::MAX_SSL_ACCEPT_COUNTER;

use super::{SslAcceptor as IoSslAcceptor, SslFilter};

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
    type Future<'f> = Ready<Self::Service, Self::InitError>;

    #[inline]
    fn create(&self, _: C) -> Self::Future<'_> {
        MAX_SSL_ACCEPT_COUNTER.with(|conns| {
            Ready::Ok(AcceptorService {
                acceptor: self.acceptor.clone(),
                conns: conns.clone(),
                _t: PhantomData,
            })
        })
    }
}

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
    type Future<'f> = AcceptorServiceResponse<F>;

    #[inline]
    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if self.conns.available(cx) {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }

    #[inline]
    fn call<'a>(&'a self, req: Io<F>, _: Ctx<'a, Self>) -> Self::Future<'a> {
        AcceptorServiceResponse {
            _guard: self.conns.get(),
            fut: self.acceptor.clone().create(req),
        }
    }
}

pin_project_lite::pin_project! {
    pub struct AcceptorServiceResponse<F>
    where
        F: Filter,
    {
        #[pin]
        fut: <IoSslAcceptor as FilterFactory<F>>::Future,
        _guard: CounterGuard,
    }
}

impl<F: Filter> Future for AcceptorServiceResponse<F> {
    type Output = Result<Io<Layer<SslFilter, F>>, Box<dyn Error>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.project().fut.poll(cx)
    }
}
