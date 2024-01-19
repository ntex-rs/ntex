use std::task::{Context, Poll};
use std::{collections::VecDeque, fmt, future::Future, io, net::SocketAddr, pin::Pin};

use ntex_bytes::{PoolId, PoolRef};
use ntex_io::{types, Io};
use ntex_service::{Service, ServiceCtx, ServiceFactory};
use ntex_util::future::{BoxFuture, Either};

use crate::{net::tcp_connect_in, Address, Connect, ConnectError, Resolver};

#[derive(Copy)]
pub struct Connector<T> {
    resolver: Resolver<T>,
    pool: PoolRef,
    tag: &'static str,
}

impl<T> Connector<T> {
    /// Construct new connect service with default dns resolver
    pub fn new() -> Self {
        Connector {
            resolver: Resolver::new(),
            pool: PoolId::P0.pool_ref(),
            tag: "TCP-CLIENT",
        }
    }

    /// Set memory pool
    ///
    /// Use specified memory pool for memory allocations. By default P0
    /// memory pool is used.
    pub fn memory_pool(mut self, id: PoolId) -> Self {
        self.pool = id.pool_ref();
        self
    }

    /// Set io tag
    ///
    /// Set tag to opened io object.
    pub fn tag(mut self, tag: &'static str) -> Self {
        self.tag = tag;
        self
    }
}

impl<T: Address> Connector<T> {
    /// Resolve and connect to remote host
    pub async fn connect<U>(&self, message: U) -> Result<Io, ConnectError>
    where
        Connect<T>: From<U>,
    {
        // resolve first
        let address = self
            .resolver
            .lookup_with_tag(message.into(), self.tag)
            .await?;

        let port = address.port();
        let Connect { req, addr, .. } = address;

        if let Some(addr) = addr {
            TcpConnectorResponse::new(req, port, addr, self.tag, self.pool).await
        } else if let Some(addr) = req.addr() {
            TcpConnectorResponse::new(
                req,
                addr.port(),
                Either::Left(addr),
                self.tag,
                self.pool,
            )
            .await
        } else {
            log::error!("{}: TCP connector: got unresolved address", self.tag);
            Err(ConnectError::Unresolved)
        }
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
            tag: self.tag,
            pool: self.pool,
        }
    }
}

impl<T> fmt::Debug for Connector<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Connector")
            .field("tag", &self.tag)
            .field("resolver", &self.resolver)
            .field("memory_pool", &self.pool)
            .finish()
    }
}

impl<T: Address, C> ServiceFactory<Connect<T>, C> for Connector<T> {
    type Response = Io;
    type Error = ConnectError;
    type Service = Connector<T>;
    type InitError = ();

    async fn create(&self, _: C) -> Result<Self::Service, Self::InitError> {
        Ok(self.clone())
    }
}

impl<T: Address> Service<Connect<T>> for Connector<T> {
    type Response = Io;
    type Error = ConnectError;

    async fn call(
        &self,
        req: Connect<T>,
        _: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        self.connect(req).await
    }
}

/// Tcp stream connector response future
struct TcpConnectorResponse<T> {
    req: Option<T>,
    port: u16,
    addrs: Option<VecDeque<SocketAddr>>,
    #[allow(clippy::type_complexity)]
    stream: Option<BoxFuture<'static, Result<Io, io::Error>>>,
    tag: &'static str,
    pool: PoolRef,
}

impl<T: Address> TcpConnectorResponse<T> {
    fn new(
        req: T,
        port: u16,
        addr: Either<SocketAddr, VecDeque<SocketAddr>>,
        tag: &'static str,
        pool: PoolRef,
    ) -> TcpConnectorResponse<T> {
        log::trace!(
            "{}: TCP connector - connecting to {:?} addr:{:?} port:{}",
            tag,
            req.host(),
            addr,
            port
        );

        match addr {
            Either::Left(addr) => TcpConnectorResponse {
                req: Some(req),
                addrs: None,
                stream: Some(Box::pin(tcp_connect_in(addr, pool))),
                tag,
                pool,
                port,
            },
            Either::Right(addrs) => TcpConnectorResponse {
                tag,
                port,
                pool,
                req: Some(req),
                addrs: Some(addrs),
                stream: None,
            },
        }
    }

    fn can_continue(&self, err: &io::Error) -> bool {
        log::trace!(
            "{}: TCP connector - failed to connect to {:?} port: {} err: {:?}",
            self.tag,
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
                        log::trace!(
                            "{}: TCP connector - successfully connected to connecting to {:?} - {:?}",
                            this.tag,
                            req.host(),
                            sock.query::<types::PeerAddr>().get()
                        );
                        sock.set_tag(this.tag);
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

        let srv = Connector::default().tag("T").memory_pool(PoolId::P5);
        let result = srv.connect("").await;
        assert!(result.is_err());
        let result = srv.connect("localhost:99999").await;
        assert!(result.is_err());
        assert!(format!("{:?}", srv).contains("Connector"));

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
