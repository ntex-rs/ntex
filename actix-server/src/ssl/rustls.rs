use std::future::Future;
use std::io;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use actix_service::{Service, ServiceFactory};
use futures::future::{ok, Ready};
use rust_tls::ServerConfig;
use tokio_io::{AsyncRead, AsyncWrite};
use tokio_rustls::{server::TlsStream, Accept, TlsAcceptor};

use crate::counter::{Counter, CounterGuard};
use crate::ssl::MAX_CONN_COUNTER;
use crate::{Io, Protocol, ServerConfig as SrvConfig};

/// Support `SSL` connections via rustls package
///
/// `rust-tls` feature enables `RustlsAcceptor` type
pub struct RustlsAcceptor<T, P = ()> {
    config: Arc<ServerConfig>,
    io: PhantomData<(T, P)>,
}

impl<T: AsyncRead + AsyncWrite, P> RustlsAcceptor<T, P> {
    /// Create `RustlsAcceptor` new service
    pub fn new(config: ServerConfig) -> Self {
        RustlsAcceptor {
            config: Arc::new(config),
            io: PhantomData,
        }
    }
}

impl<T, P> Clone for RustlsAcceptor<T, P> {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            io: PhantomData,
        }
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin, P: Unpin> ServiceFactory for RustlsAcceptor<T, P> {
    type Request = Io<T, P>;
    type Response = Io<TlsStream<T>, P>;
    type Error = io::Error;

    type Config = SrvConfig;
    type Service = RustlsAcceptorService<T, P>;
    type InitError = ();
    type Future = Ready<Result<Self::Service, Self::InitError>>;

    fn new_service(&self, cfg: &SrvConfig) -> Self::Future {
        cfg.set_secure();

        MAX_CONN_COUNTER.with(|conns| {
            ok(RustlsAcceptorService {
                acceptor: self.config.clone().into(),
                conns: conns.clone(),
                io: PhantomData,
            })
        })
    }
}

pub struct RustlsAcceptorService<T, P> {
    acceptor: TlsAcceptor,
    io: PhantomData<(T, P)>,
    conns: Counter,
}

impl<T: AsyncRead + AsyncWrite + Unpin, P: Unpin> Service for RustlsAcceptorService<T, P> {
    type Request = Io<T, P>;
    type Response = Io<TlsStream<T>, P>;
    type Error = io::Error;
    type Future = RustlsAcceptorServiceFut<T, P>;

    fn poll_ready(&mut self, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        if self.conns.available(cx) {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }

    fn call(&mut self, req: Self::Request) -> Self::Future {
        let (io, params, _) = req.into_parts();
        RustlsAcceptorServiceFut {
            _guard: self.conns.get(),
            fut: self.acceptor.accept(io),
            params: Some(params),
        }
    }
}

pub struct RustlsAcceptorServiceFut<T, P>
where
    T: AsyncRead + AsyncWrite + Unpin,
    P: Unpin,
{
    fut: Accept<T>,
    params: Option<P>,
    _guard: CounterGuard,
}

impl<T: AsyncRead + AsyncWrite + Unpin, P: Unpin> Future for RustlsAcceptorServiceFut<T, P> {
    type Output = Result<Io<TlsStream<T>, P>, io::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let this = self.get_mut();
        let params = this.params.take().unwrap();
        Poll::Ready(
            futures::ready!(Pin::new(&mut this.fut).poll(cx))
                .map(move |io| Io::from_parts(io, params, Protocol::Unknown)),
        )
    }
}
