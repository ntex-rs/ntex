use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use either::Either;
use futures::future::{ok, Ready};
use trust_dns_resolver::AsyncResolver;

use crate::rt::net::TcpStream;
use crate::service::{Service, ServiceFactory};

use super::connect::{Address, Connect, Connection};
use super::connector::{TcpConnector, TcpConnectorFactory};
use super::error::ConnectError;
use super::resolve::{Resolver, ResolverFactory};

pub struct ConnectServiceFactory<T> {
    tcp: TcpConnectorFactory<T>,
    resolver: ResolverFactory<T>,
}

impl<T> ConnectServiceFactory<T> {
    /// Construct new ConnectService factory
    pub fn new() -> Self {
        ConnectServiceFactory {
            tcp: TcpConnectorFactory::default(),
            resolver: ResolverFactory::default(),
        }
    }

    /// Construct new connect service with custom dns resolver
    pub fn with_resolver(resolver: AsyncResolver) -> Self {
        ConnectServiceFactory {
            tcp: TcpConnectorFactory::default(),
            resolver: ResolverFactory::new(resolver),
        }
    }

    /// Construct new service
    pub fn service(&self) -> ConnectService<T> {
        ConnectService {
            tcp: self.tcp.service(),
            resolver: self.resolver.service(),
        }
    }

    /// Construct new tcp stream service
    pub fn tcp_service(&self) -> TcpConnectService<T> {
        TcpConnectService {
            tcp: self.tcp.service(),
            resolver: self.resolver.service(),
        }
    }
}

impl<T> Default for ConnectServiceFactory<T> {
    fn default() -> Self {
        ConnectServiceFactory {
            tcp: TcpConnectorFactory::default(),
            resolver: ResolverFactory::default(),
        }
    }
}

impl<T> Clone for ConnectServiceFactory<T> {
    fn clone(&self) -> Self {
        ConnectServiceFactory {
            tcp: self.tcp.clone(),
            resolver: self.resolver.clone(),
        }
    }
}

impl<T: Address> ServiceFactory for ConnectServiceFactory<T> {
    type Request = Connect<T>;
    type Response = Connection<T, TcpStream>;
    type Error = ConnectError;
    type Config = ();
    type Service = ConnectService<T>;
    type InitError = ();
    type Future = Ready<Result<Self::Service, Self::InitError>>;

    fn new_service(&self, _: ()) -> Self::Future {
        ok(self.service())
    }
}

#[derive(Clone)]
pub struct ConnectService<T> {
    tcp: TcpConnector<T>,
    resolver: Resolver<T>,
}

impl<T: Address> Service for ConnectService<T> {
    type Request = Connect<T>;
    type Response = Connection<T, TcpStream>;
    type Error = ConnectError;
    type Future = ConnectServiceResponse<T>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Connect<T>) -> Self::Future {
        ConnectServiceResponse {
            state: ConnectState::Resolve(self.resolver.call(req)),
            tcp: self.tcp.clone(),
        }
    }
}

enum ConnectState<T: Address> {
    Resolve(<Resolver<T> as Service>::Future),
    Connect(<TcpConnector<T> as Service>::Future),
}

impl<T: Address> ConnectState<T> {
    fn poll(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Either<Poll<Result<Connection<T, TcpStream>, ConnectError>>, Connect<T>> {
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
    tcp: TcpConnector<T>,
}

impl<T: Address> Future for ConnectServiceResponse<T> {
    type Output = Result<Connection<T, TcpStream>, ConnectError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let res = match self.state.poll(cx) {
            Either::Right(res) => {
                self.state = ConnectState::Connect(self.tcp.call(res));
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

#[derive(Clone)]
pub struct TcpConnectService<T> {
    tcp: TcpConnector<T>,
    resolver: Resolver<T>,
}

impl<T: Address + 'static> Service for TcpConnectService<T> {
    type Request = Connect<T>;
    type Response = TcpStream;
    type Error = ConnectError;
    type Future = TcpConnectServiceResponse<T>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Connect<T>) -> Self::Future {
        TcpConnectServiceResponse {
            state: TcpConnectState::Resolve(self.resolver.call(req)),
            tcp: self.tcp.clone(),
        }
    }
}

enum TcpConnectState<T: Address> {
    Resolve(<Resolver<T> as Service>::Future),
    Connect(<TcpConnector<T> as Service>::Future),
}

impl<T: Address> TcpConnectState<T> {
    fn poll(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Either<Poll<Result<TcpStream, ConnectError>>, Connect<T>> {
        match self {
            TcpConnectState::Resolve(ref mut fut) => match Pin::new(fut).poll(cx) {
                Poll::Pending => (),
                Poll::Ready(Ok(res)) => return Either::Right(res),
                Poll::Ready(Err(err)) => return Either::Left(Poll::Ready(Err(err))),
            },
            TcpConnectState::Connect(ref mut fut) => {
                if let Poll::Ready(res) = Pin::new(fut).poll(cx) {
                    return match res {
                        Ok(conn) => Either::Left(Poll::Ready(Ok(conn.into_parts().0))),
                        Err(err) => Either::Left(Poll::Ready(Err(err))),
                    };
                }
            }
        }
        Either::Left(Poll::Pending)
    }
}

pub struct TcpConnectServiceResponse<T: Address> {
    state: TcpConnectState<T>,
    tcp: TcpConnector<T>,
}

impl<T: Address> Future for TcpConnectServiceResponse<T> {
    type Output = Result<TcpStream, ConnectError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let res = match self.state.poll(cx) {
            Either::Right(res) => {
                self.state = TcpConnectState::Connect(self.tcp.call(res));
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
