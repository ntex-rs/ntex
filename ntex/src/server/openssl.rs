use std::error::Error;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use std::{fmt, io};

pub use open_ssl::ssl::{AlpnError, Ssl, SslAcceptor, SslAcceptorBuilder};
pub use tokio_openssl::SslStream;

use futures::future::{ok, FutureExt, LocalBoxFuture, Ready};

use crate::codec::{AsyncRead, AsyncWrite};
use crate::rt::time::{sleep, Sleep};
use crate::service::{Service, ServiceFactory};
use crate::util::counter::{Counter, CounterGuard};

use super::{MAX_SSL_ACCEPT_COUNTER, ZERO};

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
    type Future = Ready<Result<Self::Service, Self::InitError>>;

    fn new_service(&self, _: ()) -> Self::Future {
        MAX_SSL_ACCEPT_COUNTER.with(|conns| {
            ok(AcceptorService {
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
            delay: if self.timeout == ZERO {
                None
            } else {
                Some(sleep(self.timeout))
            },
            fut: async move {
                let mut io = SslStream::new(ssl, req)?;
                Pin::new(&mut io).accept().await.map_err(|e| {
                    let e: Box<dyn Error> = Box::new(e);
                    e
                })?;
                Ok(io)
            }
            .boxed_local(),
        }
    }
}

pin_project_lite::pin_project! {
    pub struct AcceptorServiceResponse<T>
    where
        T: AsyncRead,
        T: AsyncWrite,
    {
        fut: LocalBoxFuture<'static, Result<SslStream<T>, Box<dyn Error>>>,
        #[pin]
        delay: Option<Sleep>,
        _guard: CounterGuard,
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin> Future for AcceptorServiceResponse<T> {
    type Output = Result<SslStream<T>, Box<dyn Error>>;

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

        let io = futures::ready!(Pin::new(&mut this.fut).poll(cx))?;
        Poll::Ready(Ok(io))
    }
}
