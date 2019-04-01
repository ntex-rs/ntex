use std::collections::VecDeque;
use std::marker::PhantomData;
use std::net::SocketAddr;

use actix_service::{NewService, Service};
use futures::future::{err, ok, Either, FutureResult};
use futures::{Async, Future, Poll};
use tokio_tcp::{ConnectFuture, TcpStream};

use super::connect::{Address, Connect, Connection};
use super::error::ConnectError;

/// Tcp connector service factory
#[derive(Debug)]
pub struct TcpConnectorFactory<T>(PhantomData<T>);

impl<T> TcpConnectorFactory<T> {
    pub fn new() -> Self {
        TcpConnectorFactory(PhantomData)
    }
}

impl<T> Clone for TcpConnectorFactory<T> {
    fn clone(&self) -> Self {
        TcpConnectorFactory(PhantomData)
    }
}

impl<T: Address> NewService for TcpConnectorFactory<T> {
    type Request = Connect<T>;
    type Response = Connection<T, TcpStream>;
    type Error = ConnectError;
    type Service = TcpConnector<T>;
    type InitError = ();
    type Future = FutureResult<Self::Service, Self::InitError>;

    fn new_service(&self, _: &()) -> Self::Future {
        ok(TcpConnector(PhantomData))
    }
}

/// Tcp connector service
#[derive(Debug)]
pub struct TcpConnector<T>(PhantomData<T>);

impl<T> TcpConnector<T> {
    pub fn new() -> Self {
        TcpConnector(PhantomData)
    }
}

impl<T> Clone for TcpConnector<T> {
    fn clone(&self) -> Self {
        TcpConnector(PhantomData)
    }
}

impl<T: Address> Service for TcpConnector<T> {
    type Request = Connect<T>;
    type Response = Connection<T, TcpStream>;
    type Error = ConnectError;
    type Future = Either<TcpConnectorResponse<T>, FutureResult<Self::Response, Self::Error>>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(Async::Ready(()))
    }

    fn call(&mut self, req: Connect<T>) -> Self::Future {
        let port = req.port();
        let Connect { req, addr, .. } = req;

        if let Some(addr) = addr {
            Either::A(TcpConnectorResponse::new(req, port, addr))
        } else {
            error!("TCP connector: got unresolved address");
            Either::B(err(ConnectError::Unresolverd))
        }
    }
}

#[doc(hidden)]
/// Tcp stream connector response future
pub struct TcpConnectorResponse<T> {
    req: Option<T>,
    port: u16,
    addrs: Option<VecDeque<SocketAddr>>,
    stream: Option<ConnectFuture>,
}

impl<T: Address> TcpConnectorResponse<T> {
    pub fn new(
        req: T,
        port: u16,
        addr: either::Either<SocketAddr, VecDeque<SocketAddr>>,
    ) -> TcpConnectorResponse<T> {
        trace!(
            "TCP connector - connecting to {:?} port:{}",
            req.host(),
            port
        );

        match addr {
            either::Either::Left(addr) => TcpConnectorResponse {
                req: Some(req),
                port,
                addrs: None,
                stream: Some(TcpStream::connect(&addr)),
            },
            either::Either::Right(addrs) => TcpConnectorResponse {
                req: Some(req),
                port,
                addrs: Some(addrs),
                stream: None,
            },
        }
    }
}

impl<T: Address> Future for TcpConnectorResponse<T> {
    type Item = Connection<T, TcpStream>;
    type Error = ConnectError;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        // connect
        loop {
            if let Some(new) = self.stream.as_mut() {
                match new.poll() {
                    Ok(Async::Ready(sock)) => {
                        let req = self.req.take().unwrap();
                        trace!(
                            "TCP connector - successfully connected to connecting to {:?} - {:?}",
                            req.host(), sock.peer_addr()
                        );
                        return Ok(Async::Ready(Connection::new(sock, req)));
                    }
                    Ok(Async::NotReady) => return Ok(Async::NotReady),
                    Err(err) => {
                        trace!(
                            "TCP connector - failed to connect to connecting to {:?} port: {}",
                            self.req.as_ref().unwrap().host(),
                            self.port,
                        );
                        if self.addrs.is_none() || self.addrs.as_ref().unwrap().is_empty() {
                            return Err(err.into());
                        }
                    }
                }
            }

            // try to connect
            self.stream = Some(TcpStream::connect(
                &self.addrs.as_mut().unwrap().pop_front().unwrap(),
            ));
        }
    }
}
