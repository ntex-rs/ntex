use std::future::Future;
use std::io;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures::future::{ok, Ready};
use tokio_rustls::{Accept, TlsAcceptor};

pub use rust_tls::{ServerConfig, Session};
pub use tokio_rustls::server::TlsStream;
pub use webpki_roots::TLS_SERVER_ROOTS;

use crate::codec::{AsyncRead, AsyncWrite};
use crate::service::{Service, ServiceFactory};
use crate::util::counter::{Counter, CounterGuard};

use super::MAX_CONN_COUNTER;

/// Support `SSL` connections via rustls package
///
/// `rust-tls` feature enables `RustlsAcceptor` type
pub struct Acceptor<T> {
    config: Arc<ServerConfig>,
    io: PhantomData<T>,
}

impl<T: AsyncRead + AsyncWrite> Acceptor<T> {
    /// Create rustls based `Acceptor` service factory
    pub fn new(config: ServerConfig) -> Self {
        Acceptor {
            config: Arc::new(config),
            io: PhantomData,
        }
    }
}

impl<T> Clone for Acceptor<T> {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            io: PhantomData,
        }
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin> ServiceFactory for Acceptor<T> {
    type Request = T;
    type Response = TlsStream<T>;
    type Error = io::Error;
    type Service = AcceptorService<T>;

    type Config = ();
    type InitError = ();
    type Future = Ready<Result<Self::Service, Self::InitError>>;

    fn new_service(&self, _: ()) -> Self::Future {
        MAX_CONN_COUNTER.with(|conns| {
            ok(AcceptorService {
                acceptor: self.config.clone().into(),
                conns: conns.clone(),
                io: PhantomData,
            })
        })
    }
}

/// RusTLS based `Acceptor` service
pub struct AcceptorService<T> {
    acceptor: TlsAcceptor,
    io: PhantomData<T>,
    conns: Counter,
}

impl<T: AsyncRead + AsyncWrite + Unpin> Service for AcceptorService<T> {
    type Request = T;
    type Response = TlsStream<T>;
    type Error = io::Error;
    type Future = AcceptorServiceFut<T>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if self.conns.available(cx) {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }

    fn call(&mut self, req: Self::Request) -> Self::Future {
        AcceptorServiceFut {
            _guard: self.conns.get(),
            fut: self.acceptor.accept(req),
        }
    }
}

pub struct AcceptorServiceFut<T>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    fut: Accept<T>,
    _guard: CounterGuard,
}

impl<T: AsyncRead + AsyncWrite + Unpin> Future for AcceptorServiceFut<T> {
    type Output = Result<TlsStream<T>, io::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        let res = futures::ready!(Pin::new(&mut this.fut).poll(cx));
        match res {
            Ok(io) => Poll::Ready(Ok(io)),
            Err(e) => Poll::Ready(Err(e)),
        }
    }
}
