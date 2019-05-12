use std::io;
use std::marker::PhantomData;

use actix_service::{NewService, Service};
use futures::{future::ok, future::FutureResult, Async, Future, Poll};
use native_tls::{self, Error, HandshakeError, TlsAcceptor};
use tokio_io::{AsyncRead, AsyncWrite};

use crate::counter::{Counter, CounterGuard};
use crate::ssl::MAX_CONN_COUNTER;
use crate::{Io, Protocol, ServerConfig};

/// Support `SSL` connections via native-tls package
///
/// `tls` feature enables `NativeTlsAcceptor` type
pub struct NativeTlsAcceptor<T, P = ()> {
    acceptor: TlsAcceptor,
    io: PhantomData<(T, P)>,
}

impl<T: AsyncRead + AsyncWrite, P> NativeTlsAcceptor<T, P> {
    /// Create `NativeTlsAcceptor` instance
    pub fn new(acceptor: TlsAcceptor) -> Self {
        NativeTlsAcceptor {
            acceptor,
            io: PhantomData,
        }
    }
}

impl<T: AsyncRead + AsyncWrite, P> Clone for NativeTlsAcceptor<T, P> {
    fn clone(&self) -> Self {
        Self {
            acceptor: self.acceptor.clone(),
            io: PhantomData,
        }
    }
}

impl<T: AsyncRead + AsyncWrite, P> NewService for NativeTlsAcceptor<T, P> {
    type Request = Io<T, P>;
    type Response = Io<TlsStream<T>, P>;
    type Error = Error;

    type Config = ServerConfig;
    type Service = NativeTlsAcceptorService<T, P>;
    type InitError = ();
    type Future = FutureResult<Self::Service, Self::InitError>;

    fn new_service(&self, cfg: &ServerConfig) -> Self::Future {
        cfg.set_secure();

        MAX_CONN_COUNTER.with(|conns| {
            ok(NativeTlsAcceptorService {
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

impl<T: AsyncRead + AsyncWrite, P> Service for NativeTlsAcceptorService<T, P> {
    type Request = Io<T, P>;
    type Response = Io<TlsStream<T>, P>;
    type Error = Error;
    type Future = Accept<T, P>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        if self.conns.available() {
            Ok(Async::Ready(()))
        } else {
            Ok(Async::NotReady)
        }
    }

    fn call(&mut self, req: Self::Request) -> Self::Future {
        let (io, params, _) = req.into_parts();
        Accept {
            _guard: self.conns.get(),
            inner: Some(self.acceptor.accept(io)),
            params: Some(params),
        }
    }
}

/// A wrapper around an underlying raw stream which implements the TLS or SSL
/// protocol.
///
/// A `TlsStream<S>` represents a handshake that has been completed successfully
/// and both the server and the client are ready for receiving and sending
/// data. Bytes read from a `TlsStream` are decrypted from `S` and bytes written
/// to a `TlsStream` are encrypted when passing through to `S`.
#[derive(Debug)]
pub struct TlsStream<S> {
    inner: native_tls::TlsStream<S>,
}

/// Future returned from `NativeTlsAcceptor::accept` which will resolve
/// once the accept handshake has finished.
pub struct Accept<S, P> {
    inner: Option<Result<native_tls::TlsStream<S>, HandshakeError<S>>>,
    params: Option<P>,
    _guard: CounterGuard,
}

impl<T: AsyncRead + AsyncWrite, P> Future for Accept<T, P> {
    type Item = Io<TlsStream<T>, P>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self.inner.take().expect("cannot poll MidHandshake twice") {
            Ok(stream) => Ok(Async::Ready(Io::from_parts(
                TlsStream { inner: stream },
                self.params.take().unwrap(),
                Protocol::Unknown,
            ))),
            Err(HandshakeError::Failure(e)) => Err(e),
            Err(HandshakeError::WouldBlock(s)) => match s.handshake() {
                Ok(stream) => Ok(Async::Ready(Io::from_parts(
                    TlsStream { inner: stream },
                    self.params.take().unwrap(),
                    Protocol::Unknown,
                ))),
                Err(HandshakeError::Failure(e)) => Err(e),
                Err(HandshakeError::WouldBlock(s)) => {
                    self.inner = Some(Err(HandshakeError::WouldBlock(s)));
                    Ok(Async::NotReady)
                }
            },
        }
    }
}

impl<S> TlsStream<S> {
    /// Get access to the internal `native_tls::TlsStream` stream which also
    /// transitively allows access to `S`.
    pub fn get_ref(&self) -> &native_tls::TlsStream<S> {
        &self.inner
    }

    /// Get mutable access to the internal `native_tls::TlsStream` stream which
    /// also transitively allows mutable access to `S`.
    pub fn get_mut(&mut self) -> &mut native_tls::TlsStream<S> {
        &mut self.inner
    }
}

impl<S: io::Read + io::Write> io::Read for TlsStream<S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl<S: io::Read + io::Write> io::Write for TlsStream<S> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl<S: AsyncRead + AsyncWrite> AsyncRead for TlsStream<S> {}

impl<S: AsyncRead + AsyncWrite> AsyncWrite for TlsStream<S> {
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        match self.inner.shutdown() {
            Ok(_) => (),
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => (),
            Err(e) => return Err(e),
        }
        self.inner.get_mut().shutdown()
    }
}
