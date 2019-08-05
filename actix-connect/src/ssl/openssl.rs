use std::marker::PhantomData;
use std::{fmt, io};

use actix_codec::{AsyncRead, AsyncWrite};
use actix_service::{NewService, Service};
use futures::{future::ok, future::FutureResult, try_ready, Async, Future, Poll};
use openssl::ssl::{HandshakeError, SslConnector};
use tokio_openssl::{ConnectAsync, SslConnectorExt, SslStream};
use tokio_tcp::TcpStream;
use trust_dns_resolver::AsyncResolver;

use crate::{
    Address, Connect, ConnectError, ConnectService, ConnectServiceFactory, Connection,
};

/// Openssl connector factory
pub struct OpensslConnector<T, U> {
    connector: SslConnector,
    _t: PhantomData<(T, U)>,
}

impl<T, U> OpensslConnector<T, U> {
    pub fn new(connector: SslConnector) -> Self {
        OpensslConnector {
            connector,
            _t: PhantomData,
        }
    }
}

impl<T, U> OpensslConnector<T, U>
where
    T: Address,
    U: AsyncRead + AsyncWrite + fmt::Debug,
{
    pub fn service(
        connector: SslConnector,
    ) -> impl Service<
        Request = Connection<T, U>,
        Response = Connection<T, SslStream<U>>,
        Error = HandshakeError<U>,
    > {
        OpensslConnectorService {
            connector: connector,
            _t: PhantomData,
        }
    }
}

impl<T, U> Clone for OpensslConnector<T, U> {
    fn clone(&self) -> Self {
        Self {
            connector: self.connector.clone(),
            _t: PhantomData,
        }
    }
}

impl<T: Address, U> NewService for OpensslConnector<T, U>
where
    U: AsyncRead + AsyncWrite + fmt::Debug,
{
    type Request = Connection<T, U>;
    type Response = Connection<T, SslStream<U>>;
    type Error = HandshakeError<U>;
    type Config = ();
    type Service = OpensslConnectorService<T, U>;
    type InitError = ();
    type Future = FutureResult<Self::Service, Self::InitError>;

    fn new_service(&self, _: &()) -> Self::Future {
        ok(OpensslConnectorService {
            connector: self.connector.clone(),
            _t: PhantomData,
        })
    }
}

pub struct OpensslConnectorService<T, U> {
    connector: SslConnector,
    _t: PhantomData<(T, U)>,
}

impl<T, U> Clone for OpensslConnectorService<T, U> {
    fn clone(&self) -> Self {
        Self {
            connector: self.connector.clone(),
            _t: PhantomData,
        }
    }
}

impl<T: Address, U> Service for OpensslConnectorService<T, U>
where
    U: AsyncRead + AsyncWrite + fmt::Debug,
{
    type Request = Connection<T, U>;
    type Response = Connection<T, SslStream<U>>;
    type Error = HandshakeError<U>;
    type Future = ConnectAsyncExt<T, U>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(Async::Ready(()))
    }

    fn call(&mut self, stream: Connection<T, U>) -> Self::Future {
        trace!("SSL Handshake start for: {:?}", stream.host());
        let (io, stream) = stream.replace(());
        ConnectAsyncExt {
            fut: SslConnectorExt::connect_async(&self.connector, stream.host(), io),
            stream: Some(stream),
        }
    }
}

pub struct ConnectAsyncExt<T, U> {
    fut: ConnectAsync<U>,
    stream: Option<Connection<T, ()>>,
}

impl<T: Address, U> Future for ConnectAsyncExt<T, U>
where
    U: AsyncRead + AsyncWrite + fmt::Debug,
{
    type Item = Connection<T, SslStream<U>>;
    type Error = HandshakeError<U>;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self.fut.poll().map_err(|e| {
            trace!("SSL Handshake error: {:?}", e);
            e
        })? {
            Async::Ready(stream) => {
                let s = self.stream.take().unwrap();
                trace!("SSL Handshake success: {:?}", s.host());
                Ok(Async::Ready(s.replace(stream).1))
            }
            Async::NotReady => Ok(Async::NotReady),
        }
    }
}

pub struct OpensslConnectServiceFactory<T> {
    tcp: ConnectServiceFactory<T>,
    openssl: OpensslConnector<T, TcpStream>,
}

impl<T> OpensslConnectServiceFactory<T> {
    /// Construct new OpensslConnectService factory
    pub fn new(connector: SslConnector) -> Self {
        OpensslConnectServiceFactory {
            tcp: ConnectServiceFactory::default(),
            openssl: OpensslConnector::new(connector),
        }
    }

    /// Construct new connect service with custom dns resolver
    pub fn with_resolver(connector: SslConnector, resolver: AsyncResolver) -> Self {
        OpensslConnectServiceFactory {
            tcp: ConnectServiceFactory::with_resolver(resolver),
            openssl: OpensslConnector::new(connector),
        }
    }

    /// Construct openssl connect service
    pub fn service(&self) -> OpensslConnectService<T> {
        OpensslConnectService {
            tcp: self.tcp.service(),
            openssl: OpensslConnectorService {
                connector: self.openssl.connector.clone(),
                _t: PhantomData,
            },
        }
    }
}

impl<T> Clone for OpensslConnectServiceFactory<T> {
    fn clone(&self) -> Self {
        OpensslConnectServiceFactory {
            tcp: self.tcp.clone(),
            openssl: self.openssl.clone(),
        }
    }
}

impl<T: Address> NewService for OpensslConnectServiceFactory<T> {
    type Request = Connect<T>;
    type Response = SslStream<TcpStream>;
    type Error = ConnectError;
    type Config = ();
    type Service = OpensslConnectService<T>;
    type InitError = ();
    type Future = FutureResult<Self::Service, Self::InitError>;

    fn new_service(&self, _: &()) -> Self::Future {
        ok(self.service())
    }
}

#[derive(Clone)]
pub struct OpensslConnectService<T> {
    tcp: ConnectService<T>,
    openssl: OpensslConnectorService<T, TcpStream>,
}

impl<T: Address> Service for OpensslConnectService<T> {
    type Request = Connect<T>;
    type Response = SslStream<TcpStream>;
    type Error = ConnectError;
    type Future = OpensslConnectServiceResponse<T>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(Async::Ready(()))
    }

    fn call(&mut self, req: Connect<T>) -> Self::Future {
        OpensslConnectServiceResponse {
            fut1: Some(self.tcp.call(req)),
            fut2: None,
            openssl: self.openssl.clone(),
        }
    }
}

pub struct OpensslConnectServiceResponse<T: Address> {
    fut1: Option<<ConnectService<T> as Service>::Future>,
    fut2: Option<<OpensslConnectorService<T, TcpStream> as Service>::Future>,
    openssl: OpensslConnectorService<T, TcpStream>,
}

impl<T: Address> Future for OpensslConnectServiceResponse<T> {
    type Item = SslStream<TcpStream>;
    type Error = ConnectError;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        if let Some(ref mut fut) = self.fut1 {
            let res = try_ready!(fut.poll());
            let _ = self.fut1.take();
            self.fut2 = Some(self.openssl.call(res));
        }

        if let Some(ref mut fut) = self.fut2 {
            let connect = try_ready!(fut
                .poll()
                .map_err(|e| ConnectError::Io(io::Error::new(io::ErrorKind::Other, e))));
            Ok(Async::Ready(connect.into_parts().0))
        } else {
            Ok(Async::NotReady)
        }
    }
}
