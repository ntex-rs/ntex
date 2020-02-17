use std::marker::PhantomData;
use std::task::{Context, Poll};

use actix_codec::{AsyncRead, AsyncWrite};
use actix_service::{Service, ServiceFactory};
use actix_utils::counter::Counter;
use futures::future::{self, FutureExt, LocalBoxFuture, TryFutureExt};
pub use native_tls::Error;
pub use tokio_tls::{TlsAcceptor, TlsStream};

use crate::MAX_CONN_COUNTER;

/// Support `SSL` connections via native-tls package
///
/// `tls` feature enables `NativeTlsAcceptor` type
pub struct NativeTlsAcceptor<T> {
    acceptor: TlsAcceptor,
    io: PhantomData<T>,
}

impl<T> NativeTlsAcceptor<T>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    /// Create `NativeTlsAcceptor` instance
    #[inline]
    pub fn new(acceptor: TlsAcceptor) -> Self {
        NativeTlsAcceptor {
            acceptor,
            io: PhantomData,
        }
    }
}

impl<T> Clone for NativeTlsAcceptor<T> {
    #[inline]
    fn clone(&self) -> Self {
        Self {
            acceptor: self.acceptor.clone(),
            io: PhantomData,
        }
    }
}

impl<T> ServiceFactory for NativeTlsAcceptor<T>
where
    T: AsyncRead + AsyncWrite + Unpin + 'static,
{
    type Request = T;
    type Response = TlsStream<T>;
    type Error = Error;
    type Service = NativeTlsAcceptorService<T>;

    type Config = ();
    type InitError = ();
    type Future = future::Ready<Result<Self::Service, Self::InitError>>;

    fn new_service(&self, _: ()) -> Self::Future {
        MAX_CONN_COUNTER.with(|conns| {
            future::ok(NativeTlsAcceptorService {
                acceptor: self.acceptor.clone(),
                conns: conns.clone(),
                io: PhantomData,
            })
        })
    }
}

pub struct NativeTlsAcceptorService<T> {
    acceptor: TlsAcceptor,
    io: PhantomData<T>,
    conns: Counter,
}

impl<T> Clone for NativeTlsAcceptorService<T> {
    fn clone(&self) -> Self {
        Self {
            acceptor: self.acceptor.clone(),
            io: PhantomData,
            conns: self.conns.clone(),
        }
    }
}

impl<T> Service for NativeTlsAcceptorService<T>
where
    T: AsyncRead + AsyncWrite + Unpin + 'static,
{
    type Request = T;
    type Response = TlsStream<T>;
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<TlsStream<T>, Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if self.conns.available(cx) {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }

    fn call(&mut self, req: Self::Request) -> Self::Future {
        let guard = self.conns.get();
        let this = self.clone();
        async move { this.acceptor.accept(req).await }
            .map_ok(move |io| {
                // Required to preserve `CounterGuard` until `Self::Future`
                // is completely resolved.
                let _ = guard;
                io
            })
            .boxed_local()
    }
}
