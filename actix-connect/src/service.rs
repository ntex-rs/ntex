use actix_service::{NewService, Service};
use futures::future::{ok, FutureResult};
use futures::{try_ready, Async, Future, Poll};
use tokio_tcp::TcpStream;
use trust_dns_resolver::AsyncResolver;

use crate::connect::{Address, Connect, Connection};
use crate::connector::{TcpConnector, TcpConnectorFactory};
use crate::error::ConnectError;
use crate::resolver::{Resolver, ResolverFactory};

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

impl<T: Address> NewService for ConnectServiceFactory<T> {
    type Request = Connect<T>;
    type Response = Connection<T, TcpStream>;
    type Error = ConnectError;
    type Config = ();
    type Service = ConnectService<T>;
    type InitError = ();
    type Future = FutureResult<Self::Service, Self::InitError>;

    fn new_service(&self, _: &()) -> Self::Future {
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

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(Async::Ready(()))
    }

    fn call(&mut self, req: Connect<T>) -> Self::Future {
        ConnectServiceResponse {
            fut1: Some(self.resolver.call(req)),
            fut2: None,
            tcp: self.tcp.clone(),
        }
    }
}

pub struct ConnectServiceResponse<T: Address> {
    fut1: Option<<Resolver<T> as Service>::Future>,
    fut2: Option<<TcpConnector<T> as Service>::Future>,
    tcp: TcpConnector<T>,
}

impl<T: Address> Future for ConnectServiceResponse<T> {
    type Item = Connection<T, TcpStream>;
    type Error = ConnectError;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        if let Some(ref mut fut) = self.fut1 {
            let res = try_ready!(fut.poll());
            let _ = self.fut1.take();
            self.fut2 = Some(self.tcp.call(res));
        }

        if let Some(ref mut fut) = self.fut2 {
            return fut.poll();
        }

        Ok(Async::NotReady)
    }
}
