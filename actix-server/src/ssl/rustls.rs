use std::io;
use std::marker::PhantomData;
use std::sync::Arc;

use actix_service::{NewService, Service};
use futures::{future::ok, future::FutureResult, Async, Future, Poll};
use rustls::{ServerConfig, ServerSession};
use tokio_io::{AsyncRead, AsyncWrite};
use tokio_rustls::{Accept, TlsAcceptor, TlsStream};

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

impl<T: AsyncRead + AsyncWrite, P> NewService<SrvConfig> for RustlsAcceptor<T, P> {
    type Request = Io<T, P>;
    type Response = Io<TlsStream<T, ServerSession>, P>;
    type Error = io::Error;
    type Service = RustlsAcceptorService<T, P>;
    type InitError = ();
    type Future = FutureResult<Self::Service, Self::InitError>;

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

impl<T: AsyncRead + AsyncWrite, P> Service for RustlsAcceptorService<T, P> {
    type Request = Io<T, P>;
    type Response = Io<TlsStream<T, ServerSession>, P>;
    type Error = io::Error;
    type Future = RustlsAcceptorServiceFut<T, P>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        if self.conns.available() {
            Ok(Async::Ready(()))
        } else {
            Ok(Async::NotReady)
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
    T: AsyncRead + AsyncWrite,
{
    fut: Accept<T>,
    params: Option<P>,
    _guard: CounterGuard,
}

impl<T: AsyncRead + AsyncWrite, P> Future for RustlsAcceptorServiceFut<T, P> {
    type Item = Io<TlsStream<T, ServerSession>, P>;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let io = futures::try_ready!(self.fut.poll());
        Ok(Async::Ready(Io::from_parts(
            io,
            self.params.take().unwrap(),
            Protocol::Unknown,
        )))
    }
}
