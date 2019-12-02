use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};

pub use tokio_openssl::{HandshakeError, SslStream};

use actix_codec::{AsyncRead, AsyncWrite};
use actix_service::{Service, ServiceFactory};
use actix_utils::counter::{Counter, CounterGuard};
use futures::future::{ok, FutureExt, LocalBoxFuture, Ready};
use open_ssl::ssl::SslAcceptor;

use crate::MAX_CONN_COUNTER;

/// Support `TLS` server connections via openssl package
///
/// `openssl` feature enables `Acceptor` type
pub struct Acceptor<T: AsyncRead + AsyncWrite> {
    acceptor: SslAcceptor,
    io: PhantomData<T>,
}

impl<T: AsyncRead + AsyncWrite> Acceptor<T> {
    /// Create default `OpensslAcceptor`
    pub fn new(acceptor: SslAcceptor) -> Self {
        Acceptor {
            acceptor,
            io: PhantomData,
        }
    }
}

impl<T: AsyncRead + AsyncWrite> Clone for Acceptor<T> {
    fn clone(&self) -> Self {
        Self {
            acceptor: self.acceptor.clone(),
            io: PhantomData,
        }
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin + 'static> ServiceFactory for Acceptor<T> {
    type Request = T;
    type Response = SslStream<T>;
    type Error = HandshakeError<T>;
    type Config = ();
    type Service = AcceptorService<T>;
    type InitError = ();
    type Future = Ready<Result<Self::Service, Self::InitError>>;

    fn new_service(&self, _: &()) -> Self::Future {
        MAX_CONN_COUNTER.with(|conns| {
            ok(AcceptorService {
                acceptor: self.acceptor.clone(),
                conns: conns.clone(),
                io: PhantomData,
            })
        })
    }
}

pub struct AcceptorService<T> {
    acceptor: SslAcceptor,
    conns: Counter,
    io: PhantomData<T>,
}

impl<T: AsyncRead + AsyncWrite + Unpin + 'static> Service for AcceptorService<T> {
    type Request = T;
    type Response = SslStream<T>;
    type Error = HandshakeError<T>;
    type Future = AcceptorServiceResponse<T>;

    fn poll_ready(&mut self, ctx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if self.conns.available(ctx) {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }

    fn call(&mut self, req: Self::Request) -> Self::Future {
        let acc = self.acceptor.clone();
        AcceptorServiceResponse {
            _guard: self.conns.get(),
            fut: async move {
                let acc = acc;
                tokio_openssl::accept(&acc, req).await
            }
                .boxed_local(),
        }
    }
}

pub struct AcceptorServiceResponse<T>
where
    T: AsyncRead + AsyncWrite,
{
    fut: LocalBoxFuture<'static, Result<SslStream<T>, HandshakeError<T>>>,
    _guard: CounterGuard,
}

impl<T: AsyncRead + AsyncWrite + Unpin> Future for AcceptorServiceResponse<T> {
    type Output = Result<SslStream<T>, HandshakeError<T>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let io = futures::ready!(Pin::new(&mut self.fut).poll(cx))?;
        Poll::Ready(Ok(io))
    }
}
