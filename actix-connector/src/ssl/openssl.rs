use std::fmt;
use std::marker::PhantomData;

use actix_codec::{AsyncRead, AsyncWrite};
use actix_service::{NewService, Service};
use futures::{future::ok, future::FutureResult, Async, Future, Poll};
use openssl::ssl::{HandshakeError, SslConnector};
use tokio_openssl::{ConnectAsync, SslConnectorExt, SslStream};

use crate::Stream;

/// Openssl connector factory
pub struct OpensslConnector<T, P, E> {
    connector: SslConnector,
    _t: PhantomData<(T, P, E)>,
}

impl<T, P, E> OpensslConnector<T, P, E> {
    pub fn new(connector: SslConnector) -> Self {
        OpensslConnector {
            connector,
            _t: PhantomData,
        }
    }
}

impl<T: AsyncRead + AsyncWrite + fmt::Debug, P> OpensslConnector<T, P, ()> {
    pub fn service(
        connector: SslConnector,
    ) -> impl Service<
        Request = Stream<T, P>,
        Response = Stream<SslStream<T>, P>,
        Error = HandshakeError<T>,
    > {
        OpensslConnectorService {
            connector: connector,
            _t: PhantomData,
        }
    }
}

impl<T, P, E> Clone for OpensslConnector<T, P, E> {
    fn clone(&self) -> Self {
        Self {
            connector: self.connector.clone(),
            _t: PhantomData,
        }
    }
}

impl<T, P, E> NewService<()> for OpensslConnector<T, P, E>
where
    T: AsyncRead + AsyncWrite + fmt::Debug,
{
    type Request = Stream<T, P>;
    type Response = Stream<SslStream<T>, P>;
    type Error = HandshakeError<T>;
    type Service = OpensslConnectorService<T, P>;
    type InitError = E;
    type Future = FutureResult<Self::Service, Self::InitError>;

    fn new_service(&self, _: &()) -> Self::Future {
        ok(OpensslConnectorService {
            connector: self.connector.clone(),
            _t: PhantomData,
        })
    }
}

pub struct OpensslConnectorService<T, P> {
    connector: SslConnector,
    _t: PhantomData<(T, P)>,
}

impl<T, P> Service for OpensslConnectorService<T, P>
where
    T: AsyncRead + AsyncWrite + fmt::Debug,
{
    type Request = Stream<T, P>;
    type Response = Stream<SslStream<T>, P>;
    type Error = HandshakeError<T>;
    type Future = ConnectAsyncExt<T, P>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(Async::Ready(()))
    }

    fn call(&mut self, stream: Stream<T, P>) -> Self::Future {
        trace!("SSL Handshake start for: {:?}", stream.host());
        let (io, stream) = stream.replace(());
        ConnectAsyncExt {
            fut: SslConnectorExt::connect_async(&self.connector, stream.host(), io),
            stream: Some(stream),
        }
    }
}

pub struct ConnectAsyncExt<T, P> {
    fut: ConnectAsync<T>,
    stream: Option<Stream<(), P>>,
}

impl<T, P> Future for ConnectAsyncExt<T, P>
where
    T: AsyncRead + AsyncWrite + fmt::Debug,
{
    type Item = Stream<SslStream<T>, P>;
    type Error = HandshakeError<T>;

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
