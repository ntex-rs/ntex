use std::task::{Context, Poll};
use std::{error::Error, future::Future, io, marker, pin::Pin, sync::Arc, time};

use tokio_rustls::{Accept, TlsAcceptor};

pub use rust_tls::{ServerConfig, Session};
pub use tokio_rustls::server::TlsStream;
pub use webpki_roots::TLS_SERVER_ROOTS;

use crate::codec::{AsyncRead, AsyncWrite};
use crate::rt::time::{sleep, Sleep};
use crate::service::{Service, ServiceFactory};
use crate::util::counter::{Counter, CounterGuard};
use crate::util::Ready;

use super::{MAX_SSL_ACCEPT_COUNTER, ZERO};

/// Support `SSL` connections via rustls package
///
/// `rust-tls` feature enables `RustlsAcceptor` type
pub struct Acceptor<T> {
    timeout: time::Duration,
    config: Arc<ServerConfig>,
    io: marker::PhantomData<T>,
}

impl<T: AsyncRead + AsyncWrite> Acceptor<T> {
    /// Create rustls based `Acceptor` service factory
    pub fn new(config: ServerConfig) -> Self {
        Acceptor {
            config: Arc::new(config),
            timeout: time::Duration::from_secs(5),
            io: marker::PhantomData,
        }
    }

    /// Set handshake timeout in milliseconds
    ///
    /// Default is set to 5 seconds.
    pub fn timeout(mut self, time: u64) -> Self {
        self.timeout = time::Duration::from_millis(time);
        self
    }
}

impl<T> Clone for Acceptor<T> {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            timeout: self.timeout,
            io: marker::PhantomData,
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
    type Future = Ready<Self::Service, Self::InitError>;

    fn new_service(&self, _: ()) -> Self::Future {
        MAX_SSL_ACCEPT_COUNTER.with(|conns| {
            Ready::Ok(AcceptorService {
                acceptor: self.config.clone().into(),
                conns: conns.priv_clone(),
                timeout: self.timeout,
                io: marker::PhantomData,
            })
        })
    }
}

/// RusTLS based `Acceptor` service
pub struct AcceptorService<T> {
    acceptor: TlsAcceptor,
    io: marker::PhantomData<T>,
    conns: Counter,
    timeout: time::Duration,
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
                Some(sleep(self.timeout))
            },
        }
    }
}

pin_project_lite::pin_project! {
    pub struct AcceptorServiceFut<T>
    where
        T: AsyncRead,
        T: AsyncWrite,
        T: Unpin,
    {
        fut: Accept<T>,
        #[pin]
        delay: Option<Sleep>,
        _guard: CounterGuard,
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin> Future for AcceptorServiceFut<T> {
    type Output = Result<TlsStream<T>, Box<dyn Error>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();

        if let Some(delay) = this.delay.as_pin_mut() {
            match delay.poll(cx) {
                Poll::Pending => (),
                Poll::Ready(_) => {
                    return Poll::Ready(Err(Box::new(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "ssl handshake timeout",
                    ))))
                }
            }
        }

        match Pin::new(&mut this.fut).poll(cx) {
            Poll::Ready(Ok(io)) => Poll::Ready(Ok(io)),
            Poll::Ready(Err(e)) => Poll::Ready(Err(Box::new(e))),
            Poll::Pending => Poll::Pending,
        }
    }
}
