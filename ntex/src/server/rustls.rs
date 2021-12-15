use std::task::{Context, Poll};
use std::{error::Error, future::Future, marker::PhantomData, pin::Pin};

pub use ntex_tls::rustls::{TlsAcceptor, TlsFilter};
pub use rust_tls::ServerConfig;
pub use webpki_roots::TLS_SERVER_ROOTS;

use crate::io::{Filter, FilterFactory, Io};
use crate::service::{Service, ServiceFactory};
use crate::time::Millis;
use crate::util::counter::{Counter, CounterGuard};
use crate::util::Ready;

use super::MAX_SSL_ACCEPT_COUNTER;

/// Support `SSL` connections via rustls package
///
/// `rust-tls` feature enables `RustlsAcceptor` type
pub struct Acceptor<F> {
    inner: TlsAcceptor,
    _t: PhantomData<F>,
}

impl<F> Acceptor<F> {
    /// Create rustls based `Acceptor` service factory
    pub fn new(config: ServerConfig) -> Self {
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

impl<F> Clone for Acceptor<F> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            _t: PhantomData,
        }
    }
}

impl<F: Filter> ServiceFactory for Acceptor<F> {
    type Request = Io<F>;
    type Response = Io<TlsFilter<F>>;
    type Error = Box<dyn Error>;
    type Service = AcceptorService<F>;

    type Config = ();
    type InitError = ();
    type Future = Ready<Self::Service, Self::InitError>;

    fn new_service(&self, _: ()) -> Self::Future {
        MAX_SSL_ACCEPT_COUNTER.with(|conns| {
            Ready::Ok(AcceptorService {
                acceptor: self.inner.clone(),
                conns: conns.priv_clone(),
                io: PhantomData,
            })
        })
    }
}

/// RusTLS based `Acceptor` service
pub struct AcceptorService<F> {
    acceptor: TlsAcceptor,
    io: PhantomData<F>,
    conns: Counter,
}

impl<F: Filter> Service for AcceptorService<F> {
    type Request = Io<F>;
    type Response = Io<TlsFilter<F>>;
    type Error = Box<dyn Error>;
    type Future = AcceptorServiceFut<F>;

    #[inline]
    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if self.conns.available(cx) {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }

    #[inline]
    fn call(&self, req: Self::Request) -> Self::Future {
        AcceptorServiceFut {
            _guard: self.conns.get(),
            fut: self.acceptor.clone().create(req),
        }
    }
}

pin_project_lite::pin_project! {
    pub struct AcceptorServiceFut<F>
    where
        F: Filter,
    {
        #[pin]
        fut: <TlsAcceptor as FilterFactory<F>>::Future,
        _guard: CounterGuard,
    }
}

impl<F: Filter> Future for AcceptorServiceFut<F> {
    type Output = Result<Io<TlsFilter<F>>, Box<dyn Error>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.project().fut.poll(cx)
    }
}
