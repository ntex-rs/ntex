use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use actix_codec::{AsyncRead, AsyncWrite};
use actix_service::{Service, ServiceFactory};
use futures::future::{ok, Ready};
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
    U: AsyncRead + AsyncWrite + Unpin + fmt::Debug,
{
    pub fn service(connector: Arc<ClientConfig>) -> RustlsConnectorService<T, U> {
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

impl<T: Address, U> ServiceFactory for RustlsConnector<T, U>
where
    U: AsyncRead + AsyncWrite + Unpin + fmt::Debug,
{
    type Request = Connection<T, U>;
    type Response = Connection<T, TlsStream<U>>;
    type Error = std::io::Error;
    type Config = ();
    type Service = RustlsConnectorService<T, U>;
    type InitError = ();
    type Future = Ready<Result<Self::Service, Self::InitError>>;

    fn new_service(&self, _: ()) -> Self::Future {
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

impl<T, U> Clone for RustlsConnectorService<T, U> {
    fn clone(&self) -> Self {
        Self {
            connector: self.connector.clone(),
            _t: PhantomData,
        }
    }
}

impl<T: Address, U> Service for RustlsConnectorService<T, U>
where
    U: AsyncRead + AsyncWrite + Unpin + fmt::Debug,
{
    type Request = Connection<T, U>;
    type Response = Connection<T, TlsStream<U>>;
    type Error = std::io::Error;
    type Future = ConnectAsyncExt<T, U>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, stream: Connection<T, U>) -> Self::Future {
        trace!("SSL Handshake start for: {:?}", stream.host());
        let (io, stream) = stream.replace(());
        let host = DNSNameRef::try_from_ascii_str(stream.host())
            .expect("rustls currently only handles hostname-based connections. See https://github.com/briansmith/webpki/issues/54");
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
    U: AsyncRead + AsyncWrite + Unpin + fmt::Debug,
{
    type Output = Result<Connection<T, TlsStream<U>>, std::io::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        Poll::Ready(
            futures::ready!(Pin::new(&mut this.fut).poll(cx)).map(|stream| {
                let s = this.stream.take().unwrap();
                trace!("SSL Handshake success: {:?}", s.host());
                s.replace(stream).1
            }),
        )
    }
}
