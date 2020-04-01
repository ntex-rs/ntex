use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};

use either::Either;
use futures::future::{self, err, ok, FutureExt, LocalBoxFuture, Ready};

use crate::rt::net::TcpStream;
use crate::service::{Service, ServiceFactory};

use super::connect::{Address, Connect};
use super::error::ConnectError;
use super::resolve::{AsyncResolver, Resolver};

pub struct Connector<T> {
    resolver: Resolver<T>,
}

impl<T> Connector<T> {
    /// Construct new connect service with custom dns resolver
    pub fn new(resolver: AsyncResolver) -> Self {
        Connector {
            resolver: Resolver::new(resolver),
        }
    }
}

impl<T> Default for Connector<T> {
    fn default() -> Self {
        Connector {
            resolver: Resolver::default(),
        }
    }
}

impl<T> Clone for Connector<T> {
    fn clone(&self) -> Self {
        Connector {
            resolver: self.resolver.clone(),
        }
    }
}

impl<T: Address> ServiceFactory for Connector<T> {
    type Request = Connect<T>;
    type Response = TcpStream;
    type Error = ConnectError;
    type Config = ();
    type Service = Connector<T>;
    type InitError = ();
    type Future = Ready<Result<Self::Service, Self::InitError>>;

    #[inline]
    fn new_service(&self, _: ()) -> Self::Future {
        ok(self.clone())
    }
}

impl<T: Address> Service for Connector<T> {
    type Request = Connect<T>;
    type Response = TcpStream;
    type Error = ConnectError;
    type Future = ConnectServiceResponse<T>;

    #[inline]
    fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    #[inline]
    fn call(&self, req: Connect<T>) -> Self::Future {
        ConnectServiceResponse {
            state: ConnectState::Resolve(self.resolver.lookup(req)),
        }
    }
}

enum ConnectState<T: Address> {
    Resolve(<Resolver<T> as Service>::Future),
    Connect(ConnectFut<T>),
}

impl<T: Address> ConnectState<T> {
    fn poll(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Either<Poll<Result<TcpStream, ConnectError>>, Connect<T>> {
        match self {
            ConnectState::Resolve(ref mut fut) => match Pin::new(fut).poll(cx) {
                Poll::Pending => Either::Left(Poll::Pending),
                Poll::Ready(Ok(res)) => Either::Right(res),
                Poll::Ready(Err(err)) => Either::Left(Poll::Ready(Err(err))),
            },
            ConnectState::Connect(ref mut fut) => Either::Left(Pin::new(fut).poll(cx)),
        }
    }
}

pub struct ConnectServiceResponse<T: Address> {
    state: ConnectState<T>,
}

impl<T: Address> Future for ConnectServiceResponse<T> {
    type Output = Result<TcpStream, ConnectError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let res = match self.state.poll(cx) {
            Either::Right(res) => {
                self.state = ConnectState::Connect(connect(res));
                self.state.poll(cx)
            }
            Either::Left(res) => return res,
        };

        match res {
            Either::Left(res) => res,
            Either::Right(_) => panic!(),
        }
    }
}

type ConnectFut<T> =
    future::Either<TcpConnectorResponse<T>, Ready<Result<TcpStream, ConnectError>>>;

/// Connect to remote host.
///
/// Ip address must be resolved.
fn connect<T: Address>(address: Connect<T>) -> ConnectFut<T> {
    let port = address.port();
    let Connect { req, addr, .. } = address;

    if let Some(addr) = addr {
        future::Either::Left(TcpConnectorResponse::new(req, port, addr))
    } else {
        error!("TCP connector: got unresolved address");
        future::Either::Right(err(ConnectError::Unresolverd))
    }
}

/// Tcp stream connector response future
struct TcpConnectorResponse<T> {
    req: Option<T>,
    port: u16,
    addrs: Option<VecDeque<SocketAddr>>,
    stream: Option<LocalBoxFuture<'static, Result<TcpStream, io::Error>>>,
}

impl<T: Address> TcpConnectorResponse<T> {
    fn new(
        req: T,
        port: u16,
        addr: Either<SocketAddr, VecDeque<SocketAddr>>,
    ) -> TcpConnectorResponse<T> {
        trace!(
            "TCP connector - connecting to {:?} port:{}",
            req.host(),
            port
        );

        match addr {
            Either::Left(addr) => TcpConnectorResponse {
                req: Some(req),
                port,
                addrs: None,
                stream: Some(TcpStream::connect(addr).boxed_local()),
            },
            Either::Right(addrs) => TcpConnectorResponse {
                req: Some(req),
                port,
                addrs: Some(addrs),
                stream: None,
            },
        }
    }
}

impl<T: Address> Future for TcpConnectorResponse<T> {
    type Output = Result<TcpStream, ConnectError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        // connect
        loop {
            if let Some(new) = this.stream.as_mut() {
                match new.as_mut().poll(cx) {
                    Poll::Ready(Ok(sock)) => {
                        let req = this.req.take().unwrap();
                        trace!(
                            "TCP connector - successfully connected to connecting to {:?} - {:?}",
                            req.host(), sock.peer_addr()
                        );
                        return Poll::Ready(Ok(sock));
                    }
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(err)) => {
                        trace!(
                            "TCP connector - failed to connect to connecting to {:?} port: {}",
                            this.req.as_ref().unwrap().host(),
                            this.port,
                        );
                        if this.addrs.is_none()
                            || this.addrs.as_ref().unwrap().is_empty()
                        {
                            return Poll::Ready(Err(err.into()));
                        }
                    }
                }
            }

            // try to connect
            let addr = this.addrs.as_mut().unwrap().pop_front().unwrap();
            this.stream = Some(TcpStream::connect(addr).boxed());
        }
    }
}
