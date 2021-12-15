use std::task::{Context, Poll};
use std::{error::Error, future::Future, marker::PhantomData, pin::Pin};

pub use ntex_tls::openssl::SslFilter;
pub use open_ssl::ssl::{self, AlpnError, Ssl, SslAcceptor, SslAcceptorBuilder};

use ntex_tls::openssl::SslAcceptor as IoSslAcceptor;

use crate::io::{Filter, FilterFactory, Io};
use crate::service::{Service, ServiceFactory};
use crate::time::Millis;
use crate::util::{counter::Counter, counter::CounterGuard, Ready};

use super::MAX_SSL_ACCEPT_COUNTER;

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

impl<F> Clone for Acceptor<F> {
    fn clone(&self) -> Self {
        Self {
            acceptor: self.acceptor.clone(),
            _t: PhantomData,
        }
    }
}

impl<F: Filter + 'static> ServiceFactory for Acceptor<F> {
    type Request = Io<F>;
    type Response = Io<SslFilter<F>>;
    type Error = Box<dyn Error>;
    type Config = ();
    type Service = AcceptorService<F>;
    type InitError = ();
    type Future = Ready<Self::Service, Self::InitError>;

    fn new_service(&self, _: ()) -> Self::Future {
        MAX_SSL_ACCEPT_COUNTER.with(|conns| {
            Ready::Ok(AcceptorService {
                acceptor: self.acceptor.clone(),
                conns: conns.priv_clone(),
                _t: PhantomData,
            })
        })
    }
}

pub struct AcceptorService<F> {
    acceptor: IoSslAcceptor,
    conns: Counter,
    _t: PhantomData<F>,
}

impl<F: Filter + 'static> Service for AcceptorService<F> {
    type Request = Io<F>;
    type Response = Io<SslFilter<F>>;
    type Error = Box<dyn Error>;
    type Future = AcceptorServiceResponse<F>;

    #[inline]
    fn poll_ready(&self, ctx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if self.conns.available(ctx) {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }

    #[inline]
    fn call(&self, req: Self::Request) -> Self::Future {
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
    type Output = Result<Io<SslFilter<F>>, Box<dyn Error>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.project().fut.poll(cx)
    }
}
