use std::fmt;
use std::marker::PhantomData;

use actix_codec::{AsyncRead, AsyncWrite};
use actix_service::{NewService, Service};
use futures::{future::ok, future::FutureResult, Async, Future, Poll};
use openssl::ssl::{HandshakeError, SslConnector};
use tokio_openssl::{ConnectAsync, SslConnectorExt, SslStream};

use crate::{Address, Connection};

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

impl<T: Address, U> NewService<()> for OpensslConnector<T, U>
where
    U: AsyncRead + AsyncWrite + fmt::Debug,
{
    type Request = Connection<T, U>;
    type Response = Connection<T, SslStream<U>>;
    type Error = HandshakeError<U>;
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
