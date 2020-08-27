use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};

use either::Either;
use futures::future::{ok, FutureExt, LocalBoxFuture, Ready};

use crate::rt::net::TcpStream;
use crate::service::{Service, ServiceFactory};

use super::{Address, AsyncResolver, Connect, ConnectError, Resolver};

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

impl<T: Address> Connector<T> {
    /// Resolve and connect to remote host
    pub fn connect<U>(
        &self,
        message: U,
    ) -> impl Future<Output = Result<TcpStream, ConnectError>>
    where
        Connect<T>: From<U>,
    {
        ConnectServiceResponse::new(self.resolver.lookup(message.into()))
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
        ConnectServiceResponse::new(self.resolver.lookup(req))
    }
}

enum ConnectState<T: Address> {
    Resolve(<Resolver<T> as Service>::Future),
    Connect(TcpConnectorResponse<T>),
}

#[doc(hidden)]
pub struct ConnectServiceResponse<T: Address> {
    state: ConnectState<T>,
}

impl<T: Address> ConnectServiceResponse<T> {
    pub(super) fn new(fut: <Resolver<T> as Service>::Future) -> Self {
        ConnectServiceResponse {
            state: ConnectState::Resolve(fut),
        }
    }
}

impl<T: Address> Future for ConnectServiceResponse<T> {
    type Output = Result<TcpStream, ConnectError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.state {
            ConnectState::Resolve(ref mut fut) => match Pin::new(fut).poll(cx)? {
                Poll::Pending => Poll::Pending,
                Poll::Ready(address) => {
                    let port = address.port();
                    let Connect { req, addr, .. } = address;

                    if let Some(addr) = addr {
                        self.state = ConnectState::Connect(TcpConnectorResponse::new(
                            req, port, addr,
                        ));
                        self.poll(cx)
                    } else if let Some(addr) = req.addr() {
                        self.state = ConnectState::Connect(TcpConnectorResponse::new(
                            req,
                            addr.port(),
                            Either::Left(addr),
                        ));
                        self.poll(cx)
                    } else {
                        error!("TCP connector: got unresolved address");
                        Poll::Ready(Err(ConnectError::Unresolved))
                    }
                }
            },
            ConnectState::Connect(ref mut fut) => Pin::new(fut).poll(cx),
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[ntex_rt::test]
    async fn test_connect() {
        let server = crate::server::test_server(|| {
            crate::fn_service(|_| async { Ok::<_, ()>(()) })
        });

        let srv = Connector::default();
        let result = srv.connect("").await;
        assert!(result.is_err());
        let result = srv.connect("localhost:99999").await;
        assert!(result.is_err());

        let srv = Connector::default();
        let result = srv.connect(format!("{}", server.addr())).await;
        assert!(result.is_ok());

        let msg = Connect::new(format!("{}", server.addr())).set_addrs(vec![
            format!("127.0.0.1:{}", server.addr().port() - 1)
                .parse()
                .unwrap(),
            server.addr(),
        ]);
        let result = crate::connect::connect(msg).await;
        assert!(result.is_ok());

        let msg = Connect::new(server.addr());
        let result = crate::connect::connect(msg).await;
        assert!(result.is_ok());
    }
}
