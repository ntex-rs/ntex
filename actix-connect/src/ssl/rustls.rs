use std::fmt;
use std::marker::PhantomData;

use actix_codec::{AsyncRead, AsyncWrite};
use actix_service::{NewService, Service};
use futures::{future::ok, future::FutureResult, Async, Future, Poll};
use std::sync::Arc;
use tokio_rustls::{client::TlsStream, rustls::ClientConfig, Connect, TlsConnector};
use webpki::DNSNameRef;

use crate::{Address, Connection};

/// Rustls connector factory
pub struct RustlsConnector<T, U> {
    connector: Arc<ClientConfig>,
    _t: PhantomData<(T, U)>,
}

impl<T, U> RustlsConnector<T, U> {
    pub fn new(connector: Arc<ClientConfig>) -> Self {
        RustlsConnector {
            connector,
            _t: PhantomData,
        }
    }
}

impl<T, U> RustlsConnector<T, U>
where
    T: Address,
    U: AsyncRead + AsyncWrite + fmt::Debug,
{
    pub fn service(
        connector: Arc<ClientConfig>,
    ) -> impl Service<
        Request = Connection<T, U>,
        Response = Connection<T, TlsStream<U>>,
        Error = std::io::Error,
    > {
        RustlsConnectorService {
            connector: connector,
            _t: PhantomData,
        }
    }
}

impl<T, U> Clone for RustlsConnector<T, U> {
    fn clone(&self) -> Self {
        Self {
            connector: self.connector.clone(),
            _t: PhantomData,
        }
    }
}

impl<T: Address, U> NewService for RustlsConnector<T, U>
where
    U: AsyncRead + AsyncWrite + fmt::Debug,
{
    type Request = Connection<T, U>;
    type Response = Connection<T, TlsStream<U>>;
    type Error = std::io::Error;
    type Config = ();
    type Service = RustlsConnectorService<T, U>;
    type InitError = ();
    type Future = FutureResult<Self::Service, Self::InitError>;

    fn new_service(&self, _: &()) -> Self::Future {
        ok(RustlsConnectorService {
            connector: self.connector.clone(),
            _t: PhantomData,
        })
    }
}

pub struct RustlsConnectorService<T, U> {
    connector: Arc<ClientConfig>,
    _t: PhantomData<(T, U)>,
}

impl<T: Address, U> Service for RustlsConnectorService<T, U>
where
    U: AsyncRead + AsyncWrite + fmt::Debug,
{
    type Request = Connection<T, U>;
    type Response = Connection<T, TlsStream<U>>;
    type Error = std::io::Error;
    type Future = ConnectAsyncExt<T, U>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(Async::Ready(()))
    }

    fn call(&mut self, stream: Connection<T, U>) -> Self::Future {
        trace!("SSL Handshake start for: {:?}", stream.host());
        let (io, stream) = stream.replace(());
        let host = DNSNameRef::try_from_ascii_str(stream.host()).unwrap();
        ConnectAsyncExt {
            fut: TlsConnector::from(self.connector.clone()).connect(host, io),
            stream: Some(stream),
        }
    }
}

pub struct ConnectAsyncExt<T, U> {
    fut: Connect<U>,
    stream: Option<Connection<T, ()>>,
}

impl<T: Address, U> Future for ConnectAsyncExt<T, U>
where
    U: AsyncRead + AsyncWrite + fmt::Debug,
{
    type Item = Connection<T, TlsStream<U>>;
    type Error = std::io::Error;

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
