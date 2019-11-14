use std::convert::Infallible;
use std::marker::PhantomData;
use std::task::{Context, Poll};

use actix_service::{Service, ServiceFactory};
use futures::future::{self, FutureExt as _, LocalBoxFuture, TryFutureExt as _};
use native_tls::Error;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tls::{TlsAcceptor, TlsStream};

use crate::counter::Counter;
use crate::ssl::MAX_CONN_COUNTER;
use crate::{Io, ServerConfig};

/// Support `SSL` connections via native-tls package
///
/// `tls` feature enables `NativeTlsAcceptor` type
pub struct NativeTlsAcceptor<T, P = ()> {
    acceptor: TlsAcceptor,
    io: PhantomData<(T, P)>,
}

impl<T, P> NativeTlsAcceptor<T, P>
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

impl<T, P> Clone for NativeTlsAcceptor<T, P> {
    #[inline]
    fn clone(&self) -> Self {
        Self {
            acceptor: self.acceptor.clone(),
            io: PhantomData,
        }
    }
}

impl<T, P> ServiceFactory for NativeTlsAcceptor<T, P>
where
    T: AsyncRead + AsyncWrite + Unpin + 'static,
    P: 'static,
{
    type Request = Io<T, P>;
    type Response = Io<TlsStream<T>, P>;
    type Error = Error;

    type Config = ServerConfig;
    type Service = NativeTlsAcceptorService<T, P>;
    type InitError = Infallible;
    type Future = future::Ready<Result<Self::Service, Self::InitError>>;

    fn new_service(&self, cfg: &ServerConfig) -> Self::Future {
        cfg.set_secure();

        MAX_CONN_COUNTER.with(|conns| {
            future::ok(NativeTlsAcceptorService {
                acceptor: self.acceptor.clone(),
                conns: conns.clone(),
                io: PhantomData,
            })
        })
    }
}

pub struct NativeTlsAcceptorService<T, P> {
    acceptor: TlsAcceptor,
    io: PhantomData<(T, P)>,
    conns: Counter,
}

impl<T, P> Clone for NativeTlsAcceptorService<T, P> {
    fn clone(&self) -> Self {
        Self {
            acceptor: self.acceptor.clone(),
            io: PhantomData,
            conns: self.conns.clone(),
        }
    }
}

impl<T, P> Service for NativeTlsAcceptorService<T, P>
where
    T: AsyncRead + AsyncWrite + Unpin + 'static,
    P: 'static,
{
    type Request = Io<T, P>;
    type Response = Io<TlsStream<T>, P>;
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Io<TlsStream<T>, P>, Error>>;

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
        let (io, params, proto) = req.into_parts();
        async move { this.acceptor.accept(io).await }
            .map_ok(move |stream| Io::from_parts(stream, params, proto))
            .map_ok(move |io| {
                // Required to preserve `CounterGuard` until `Self::Future`
                // is completely resolved.
                let _ = guard;
                io
            })
            .boxed_local()
    }
}
