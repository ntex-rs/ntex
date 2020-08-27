use std::error::Error;
use std::future::Future;
use std::io;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use futures::future::{ok, Ready};
use tokio_rustls::{Accept, TlsAcceptor};

pub use rust_tls::{ServerConfig, Session};
pub use tokio_rustls::server::TlsStream;
pub use webpki_roots::TLS_SERVER_ROOTS;

use crate::codec::{AsyncRead, AsyncWrite};
use crate::rt::time::{delay_for, Delay};
use crate::service::{Service, ServiceFactory};
use crate::util::counter::{Counter, CounterGuard};

use super::{MAX_SSL_ACCEPT_COUNTER, ZERO};

/// Support `SSL` connections via rustls package
///
/// `rust-tls` feature enables `RustlsAcceptor` type
pub struct Acceptor<T> {
    timeout: Duration,
    config: Arc<ServerConfig>,
    io: PhantomData<T>,
}

impl<T: AsyncRead + AsyncWrite> Acceptor<T> {
    /// Create rustls based `Acceptor` service factory
    pub fn new(config: ServerConfig) -> Self {
        Acceptor {
            config: Arc::new(config),
            timeout: Duration::from_secs(5),
            io: PhantomData,
        }
    }

    /// Set handshake timeout in milliseconds
    ///
    /// Default is set to 5 seconds.
    pub fn timeout(mut self, time: u64) -> Self {
        self.timeout = Duration::from_millis(time);
        self
    }
}

impl<T> Clone for Acceptor<T> {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            timeout: self.timeout,
            io: PhantomData,
        }
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin> ServiceFactory for Acceptor<T> {
    type Request = T;
    type Response = TlsStream<T>;
    type Error = Box<dyn Error>;
    type Service = AcceptorService<T>;

    type Config = ();
    type InitError = ();
    type Future = Ready<Result<Self::Service, Self::InitError>>;

    fn new_service(&self, _: ()) -> Self::Future {
        MAX_SSL_ACCEPT_COUNTER.with(|conns| {
            ok(AcceptorService {
                acceptor: self.config.clone().into(),
                conns: conns.priv_clone(),
                timeout: self.timeout,
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
    timeout: Duration,
}

impl<T: AsyncRead + AsyncWrite + Unpin> Service for AcceptorService<T> {
    type Request = T;
    type Response = TlsStream<T>;
    type Error = Box<dyn Error>;
    type Future = AcceptorServiceFut<T>;

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
            fut: self.acceptor.accept(req),
            delay: if self.timeout == ZERO {
                None
            } else {
                Some(delay_for(self.timeout))
            },
        }
    }
}

pub struct AcceptorServiceFut<T>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    fut: Accept<T>,
    delay: Option<Delay>,
    _guard: CounterGuard,
}

impl<T: AsyncRead + AsyncWrite + Unpin> Future for AcceptorServiceFut<T> {
    type Output = Result<TlsStream<T>, Box<dyn Error>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        if let Some(ref mut delay) = this.delay {
            match Pin::new(delay).poll(cx) {
                Poll::Pending => (),
                Poll::Ready(_) => {
                    return Poll::Ready(Err(Box::new(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "ssl handshake timeout",
                    ))))
                }
            }
        }

        let res = futures::ready!(Pin::new(&mut this.fut).poll(cx));
        match res {
            Ok(io) => Poll::Ready(Ok(io)),
            Err(e) => Poll::Ready(Err(Box::new(e))),
        }
    }
}
