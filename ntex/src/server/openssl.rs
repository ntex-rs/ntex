use std::task::{Context, Poll};
use std::{error::Error, fmt, future::Future, io, marker::PhantomData, pin::Pin};

pub use open_ssl::ssl::{self, AlpnError, Ssl, SslAcceptor, SslAcceptorBuilder};
pub use tokio_openssl::SslStream;

use crate::codec::{AsyncRead, AsyncWrite};
use crate::service::{Service, ServiceFactory};
use crate::time::{sleep, Duration, Sleep};
use crate::util::{counter::Counter, counter::CounterGuard, Ready};

use super::MAX_SSL_ACCEPT_COUNTER;

/// Support `TLS` server connections via openssl package
///
/// `openssl` feature enables `Acceptor` type
pub struct Acceptor<T: AsyncRead + AsyncWrite> {
    acceptor: SslAcceptor,
    timeout: Duration,
    io: PhantomData<T>,
}

impl<T: AsyncRead + AsyncWrite> Acceptor<T> {
    /// Create default openssl acceptor service
    pub fn new(acceptor: SslAcceptor) -> Self {
        Acceptor {
            acceptor,
            timeout: Duration::from_millis(5_000),
            io: PhantomData,
        }
    }

    /// Set handshake timeout.
    ///
    /// Default is set to 5 seconds.
    pub fn timeout<U: Into<Duration>>(mut self, timeout: U) -> Self {
        self.timeout = timeout.into();
        self
    }
}

impl<T: AsyncRead + AsyncWrite> Clone for Acceptor<T> {
    fn clone(&self) -> Self {
        Self {
            acceptor: self.acceptor.clone(),
            timeout: self.timeout,
            io: PhantomData,
        }
    }
}

impl<T> ServiceFactory for Acceptor<T>
where
    T: AsyncRead + AsyncWrite + Unpin + fmt::Debug + 'static,
{
    type Request = T;
    type Response = SslStream<T>;
    type Error = Box<dyn Error>;
    type Config = ();
    type Service = AcceptorService<T>;
    type InitError = ();
    type Future = Ready<Self::Service, Self::InitError>;

    fn new_service(&self, _: ()) -> Self::Future {
        MAX_SSL_ACCEPT_COUNTER.with(|conns| {
            Ready::Ok(AcceptorService {
                acceptor: self.acceptor.clone(),
                conns: conns.priv_clone(),
                timeout: self.timeout,
                io: PhantomData,
            })
        })
    }
}

pub struct AcceptorService<T> {
    acceptor: SslAcceptor,
    conns: Counter,
    timeout: Duration,
    io: PhantomData<T>,
}

impl<T> Service for AcceptorService<T>
where
    T: AsyncRead + AsyncWrite + Unpin + fmt::Debug + 'static,
{
    type Request = T;
    type Response = SslStream<T>;
    type Error = Box<dyn Error>;
    type Future = AcceptorServiceResponse<T>;

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
        let ssl = Ssl::new(self.acceptor.context())
            .expect("Provided SSL acceptor was invalid.");
        AcceptorServiceResponse {
            _guard: self.conns.get(),
            io: None,
            delay: self.timeout.map(|t| sleep(t)),
            io_factory: Some(SslStream::new(ssl, req)),
        }
    }
}

pub struct AcceptorServiceResponse<T>
where
    T: AsyncRead,
    T: AsyncWrite,
{
    io: Option<SslStream<T>>,
    delay: Option<Sleep>,
    io_factory: Option<Result<SslStream<T>, open_ssl::error::ErrorStack>>,
    _guard: CounterGuard,
}

impl<T: AsyncRead + AsyncWrite + Unpin> Future for AcceptorServiceResponse<T> {
    type Output = Result<SslStream<T>, Box<dyn Error>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.as_mut();

        if let Some(ref delay) = this.delay {
            match delay.poll_elapsed(cx) {
                Poll::Pending => (),
                Poll::Ready(_) => {
                    return Poll::Ready(Err(Box::new(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "ssl handshake timeout",
                    ))))
                }
            }
        }

        match this.io_factory.take() {
            Some(Ok(io)) => this.io = Some(io),
            Some(Err(err)) => return Poll::Ready(Err(Box::new(err))),
            None => (),
        }

        let io = this.io.as_mut().unwrap();
        match Pin::new(io).poll_accept(cx) {
            Poll::Ready(Ok(_)) => Poll::Ready(Ok(this.io.take().unwrap())),
            Poll::Ready(Err(e)) => Poll::Ready(Err(Box::new(e))),
            Poll::Pending => Poll::Pending,
        }
    }
}
