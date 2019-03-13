use std::collections::VecDeque;
use std::net::SocketAddr;

use actix_service::{NewService, Service};
use futures::future::{err, ok, Either, FutureResult};
use futures::{Async, Future, Poll};
use tokio_tcp::{ConnectFuture, TcpStream};

use super::connect::{Connect, Stream};
use super::error::ConnectError;

/// Tcp connector service factory
#[derive(Copy, Clone, Debug)]
pub struct ConnectorFactory;

impl NewService for ConnectorFactory {
    type Request = Connect;
    type Response = Stream<TcpStream>;
    type Error = ConnectError;
    type Service = Connector;
    type InitError = ();
    type Future = FutureResult<Self::Service, Self::InitError>;

    fn new_service(&self, _: &()) -> Self::Future {
        ok(Connector)
    }
}

/// Tcp connector service
#[derive(Copy, Clone, Debug)]
pub struct Connector;

impl Service for Connector {
    type Request = Connect;
    type Response = Stream<TcpStream>;
    type Error = ConnectError;
    type Future = Either<ConnectorResponse, FutureResult<Self::Response, Self::Error>>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(Async::Ready(()))
    }

    fn call(&mut self, req: Connect) -> Self::Future {
        match req {
            Connect::Host { .. } => {
                error!("TCP connector: got unresolved address");
                Either::B(err(ConnectError::Unresolverd))
            }
            Connect::Addr { host, addr } => Either::A(ConnectorResponse::new(host, addr)),
        }
    }
}

#[doc(hidden)]
/// Tcp stream connector response future
pub struct ConnectorResponse {
    host: Option<String>,
    addrs: Option<VecDeque<SocketAddr>>,
    stream: Option<ConnectFuture>,
}

impl ConnectorResponse {
    pub fn new(
        host: String,
        addr: either::Either<SocketAddr, VecDeque<SocketAddr>>,
    ) -> ConnectorResponse {
        trace!("TCP connector - connecting to {:?}", host);

        match addr {
            either::Either::Left(addr) => ConnectorResponse {
                host: Some(host),
                addrs: None,
                stream: Some(TcpStream::connect(&addr)),
            },
            either::Either::Right(addrs) => ConnectorResponse {
                host: Some(host),
                addrs: Some(addrs),
                stream: None,
            },
        }
    }
}

impl Future for ConnectorResponse {
    type Item = Stream<TcpStream>;
    type Error = ConnectError;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        // connect
        loop {
            if let Some(new) = self.stream.as_mut() {
                match new.poll() {
                    Ok(Async::Ready(sock)) => {
                        let host = self.host.take().unwrap();
                        trace!(
                            "TCP connector - successfully connected to connecting to {:?} - {:?}",
                            host, sock.peer_addr()
                        );
                        return Ok(Async::Ready(Stream::new(sock, host)));
                    }
                    Ok(Async::NotReady) => return Ok(Async::NotReady),
                    Err(err) => {
                        trace!(
                            "TCP connector - failed to connect to connecting to {:?}",
                            self.host.as_ref().unwrap()
                        );
                        if self.addrs.as_ref().unwrap().is_empty() {
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

// #[derive(Clone)]
// pub struct DefaultConnector(Connector);

// impl Default for DefaultConnector {
//     fn default() -> Self {
//         DefaultConnector(Connector::default())
//     }
// }

// impl DefaultConnector {
//     pub fn new(cfg: ResolverConfig, opts: ResolverOpts) -> Self {
//         DefaultConnector(Connector::new(cfg, opts))
//     }
// }

// impl Service for DefaultConnector {
//     type Request = Connect;
//     type Response = TcpStream;
//     type Error = ConnectorError;
//     type Future = DefaultConnectorFuture;

//     fn poll_ready(&mut self) -> Poll<(), Self::Error> {
//         self.0.poll_ready()
//     }

//     fn call(&mut self, req: Connect) -> Self::Future {
//         DefaultConnectorFuture {
//             fut: self.0.call(req),
//         }
//     }
// }

// #[doc(hidden)]
// pub struct DefaultConnectorFuture {
//     fut: Either<ConnectorFuture, ConnectorTcpFuture>,
// }

// impl Future for DefaultConnectorFuture {
//     type Item = TcpStream;
//     type Error = ConnectorError;

//     fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
//         Ok(Async::Ready(try_ready!(self.fut.poll()).1))
//     }
// }
