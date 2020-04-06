use std::error::Error;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use std::{fmt, io};

pub use open_ssl::ssl::{AlpnError, SslAcceptor, SslAcceptorBuilder};
pub use tokio_openssl::SslStream;

use futures::future::{ok, FutureExt, LocalBoxFuture, Ready};

use crate::codec::{AsyncRead, AsyncWrite};
use crate::rt::time::{delay_for, Delay};
use crate::service::{Service, ServiceFactory};
use crate::util::counter::{Counter, CounterGuard};

use super::{MAX_CONN_COUNTER, ZERO};

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
        MAX_CONN_COUNTER.with(|conns| {
            ok(AcceptorService {
                acceptor: self.acceptor.clone(),
                conns: conns.clone(),
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
        let acc = self.acceptor.clone();
        AcceptorServiceResponse {
            _guard: self.conns.get(),
            delay: if self.timeout == ZERO {
                None
            } else {
                Some(delay_for(self.timeout))
            },
            fut: async move {
                let acc = acc;
                tokio_openssl::accept(&acc, req).await.map_err(|e| {
                    let e: Box<dyn Error> = Box::new(e);
                    e
                })
            }
            .boxed_local(),
        }
    }
}

pub struct AcceptorServiceResponse<T>
where
    T: AsyncRead + AsyncWrite,
{
    fut: LocalBoxFuture<'static, Result<SslStream<T>, Box<dyn Error>>>,
    delay: Option<Delay>,
    _guard: CounterGuard,
}

impl<T: AsyncRead + AsyncWrite + Unpin> Future for AcceptorServiceResponse<T> {
    type Output = Result<SslStream<T>, Box<dyn Error>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(ref mut delay) = self.delay {
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

        let io = futures::ready!(Pin::new(&mut self.fut).poll(cx))?;
        Poll::Ready(Ok(io))
    }
}
