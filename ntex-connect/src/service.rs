use std::task::{Context, Poll};
use std::{collections::VecDeque, future::Future, io, net::SocketAddr, pin::Pin};

use ntex_bytes::{PoolId, PoolRef};
use ntex_io::{types, Io};
use ntex_service::{Service, ServiceCtx, ServiceFactory};
use ntex_util::future::{BoxFuture, Either, Ready};

use crate::{net::tcp_connect_in, Address, Connect, ConnectError, Resolver};

pub struct Connector<T> {
    resolver: Resolver<T>,
    pool: PoolRef,
}

impl<T> Connector<T> {
    /// Construct new connect service with custom dns resolver
    pub fn new() -> Self {
        Connector {
            resolver: Resolver::new(),
            pool: PoolId::P0.pool_ref(),
        }
    }

    /// Set memory pool.
    ///
    /// Use specified memory pool for memory allocations. By default P0
    /// memory pool is used.
    pub fn memory_pool(mut self, id: PoolId) -> Self {
        self.pool = id.pool_ref();
        self
    }
}

impl<T: Address> Connector<T> {
    /// Resolve and connect to remote host
    pub async fn connect<U>(&self, message: U) -> Result<Io, ConnectError>
    where
        Connect<T>: From<U>,
    {
        ConnectServiceResponse {
            state: ConnectState::Resolve(Box::pin(self.resolver.lookup(message.into()))),
            pool: self.pool,
        }
        .await
    }
}

impl<T> Default for Connector<T> {
    fn default() -> Self {
        Connector::new()
    }
}

impl<T> Clone for Connector<T> {
    fn clone(&self) -> Self {
        Connector {
            resolver: self.resolver.clone(),
            pool: self.pool,
        }
    }
}

impl<T: Address, C: 'static> ServiceFactory<Connect<T>, C> for Connector<T> {
    type Response = Io;
    type Error = ConnectError;
    type Service = Connector<T>;
    type InitError = ();
    type Future<'f> = Ready<Self::Service, Self::InitError> where Self: 'f;

    #[inline]
    fn create(&self, _: C) -> Self::Future<'_> {
        Ready::Ok(self.clone())
    }
}

impl<T: Address> Service<Connect<T>> for Connector<T> {
    type Response = Io;
    type Error = ConnectError;
    type Future<'f> = ConnectServiceResponse<'f, T>;

    #[inline]
    fn call<'a>(&'a self, req: Connect<T>, _: ServiceCtx<'a, Self>) -> Self::Future<'a> {
        ConnectServiceResponse::new(Box::pin(self.resolver.lookup(req)))
    }
}

enum ConnectState<'f, T: Address> {
    Resolve(BoxFuture<'f, Result<Connect<T>, ConnectError>>),
    Connect(TcpConnectorResponse<T>),
}

#[doc(hidden)]
pub struct ConnectServiceResponse<'f, T: Address> {
    state: ConnectState<'f, T>,
    pool: PoolRef,
}

impl<'f, T: Address> ConnectServiceResponse<'f, T> {
    pub(super) fn new(fut: BoxFuture<'f, Result<Connect<T>, ConnectError>>) -> Self {
        Self {
            state: ConnectState::Resolve(fut),
            pool: PoolId::P0.pool_ref(),
        }
    }
}

impl<'f, T: Address> Future for ConnectServiceResponse<'f, T> {
    type Output = Result<Io, ConnectError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.state {
            ConnectState::Resolve(ref mut fut) => match Pin::new(fut).poll(cx)? {
                Poll::Pending => Poll::Pending,
                Poll::Ready(address) => {
                    let port = address.port();
                    let Connect { req, addr, .. } = address;

                    if let Some(addr) = addr {
                        self.state = ConnectState::Connect(TcpConnectorResponse::new(
                            req, port, addr, self.pool,
                        ));
                        self.poll(cx)
                    } else if let Some(addr) = req.addr() {
                        self.state = ConnectState::Connect(TcpConnectorResponse::new(
                            req,
                            addr.port(),
                            Either::Left(addr),
                            self.pool,
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
    #[allow(clippy::type_complexity)]
    stream: Option<BoxFuture<'static, Result<Io, io::Error>>>,
    pool: PoolRef,
}

impl<T: Address> TcpConnectorResponse<T> {
    fn new(
        req: T,
        port: u16,
        addr: Either<SocketAddr, VecDeque<SocketAddr>>,
        pool: PoolRef,
    ) -> TcpConnectorResponse<T> {
        trace!(
            "TCP connector - connecting to {:?} port:{}",
            req.host(),
            port
        );

        match addr {
            Either::Left(addr) => TcpConnectorResponse {
                req: Some(req),
                addrs: None,
                stream: Some(Box::pin(tcp_connect_in(addr, pool))),
                pool,
                port,
            },
            Either::Right(addrs) => TcpConnectorResponse {
                port,
                pool,
                req: Some(req),
                addrs: Some(addrs),
                stream: None,
            },
        }
    }

    fn can_continue(&self, err: &io::Error) -> bool {
        trace!(
            "TCP connector - failed to connect to {:?} port: {} err: {:?}",
            self.req.as_ref().unwrap().host(),
            self.port,
            err
        );
        !(self.addrs.is_none() || self.addrs.as_ref().unwrap().is_empty())
    }
}

impl<T: Address> Future for TcpConnectorResponse<T> {
    type Output = Result<Io, ConnectError>;

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
                            req.host(),
                            sock.query::<types::PeerAddr>().get()
                        );
                        return Poll::Ready(Ok(sock));
                    }
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(err)) => {
                        if !this.can_continue(&err) {
                            return Poll::Ready(Err(err.into()));
                        }
                    }
                }
            }

            // try to connect
            let addr = this.addrs.as_mut().unwrap().pop_front().unwrap();
            this.stream = Some(Box::pin(tcp_connect_in(addr, this.pool)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[ntex::test]
    async fn test_connect() {
        let server = ntex::server::test_server(|| {
            ntex_service::fn_service(|_| async { Ok::<_, ()>(()) })
        });

        let srv = Connector::default().memory_pool(PoolId::P5);
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
        let result = crate::connect(msg).await;
        assert!(result.is_ok());

        let msg = Connect::new(server.addr());
        let result = crate::connect(msg).await;
        assert!(result.is_ok());
    }
}
