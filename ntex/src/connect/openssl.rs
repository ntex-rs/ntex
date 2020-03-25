use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::{fmt, io};

pub use open_ssl::ssl::{Error as SslError, SslConnector, SslMethod};
pub use tokio_openssl::{HandshakeError, SslStream};

use actix_codec::{AsyncRead, AsyncWrite};
use futures::future::{err, ok, Either, FutureExt, LocalBoxFuture, Ready};
use trust_dns_resolver::AsyncResolver;

use crate::connect::{
    Address, Connect, ConnectError, ConnectService, ConnectServiceFactory, Connection,
};
use crate::rt::net::TcpStream;
use crate::service::{Service, ServiceFactory};

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
    T: Address + 'static,
    U: AsyncRead + AsyncWrite + Unpin + fmt::Debug + 'static,
{
    pub fn service(connector: SslConnector) -> OpensslConnectorService<T, U> {
        OpensslConnectorService {
            connector,
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

impl<T, U> ServiceFactory for OpensslConnector<T, U>
where
    T: Address + 'static,
    U: AsyncRead + AsyncWrite + Unpin + fmt::Debug + 'static,
{
    type Request = Connection<T, U>;
    type Response = Connection<T, SslStream<U>>;
    type Error = io::Error;
    type Config = ();
    type Service = OpensslConnectorService<T, U>;
    type InitError = ();
    type Future = Ready<Result<Self::Service, Self::InitError>>;

    fn new_service(&self, _: ()) -> Self::Future {
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

impl<T, U> Service for OpensslConnectorService<T, U>
where
    T: Address + 'static,
    U: AsyncRead + AsyncWrite + Unpin + fmt::Debug + 'static,
{
    type Request = Connection<T, U>;
    type Response = Connection<T, SslStream<U>>;
    type Error = io::Error;
    type Future =
        Either<ConnectAsyncExt<T, U>, Ready<Result<Self::Response, Self::Error>>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, stream: Connection<T, U>) -> Self::Future {
        trace!("SSL Handshake start for: {:?}", stream.host());
        let (io, stream) = stream.replace(());
        let host = stream.host().to_string();

        match self.connector.configure() {
            Err(e) => Either::Right(err(io::Error::new(io::ErrorKind::Other, e))),
            Ok(config) => Either::Left(ConnectAsyncExt {
                fut: async move { tokio_openssl::connect(config, &host, io).await }
                    .boxed_local(),
                stream: Some(stream),
                _t: PhantomData,
            }),
        }
    }
}

pub struct ConnectAsyncExt<T, U> {
    fut: LocalBoxFuture<'static, Result<SslStream<U>, HandshakeError<U>>>,
    stream: Option<Connection<T, ()>>,
    _t: PhantomData<U>,
}

impl<T: Address, U> Future for ConnectAsyncExt<T, U>
where
    U: AsyncRead + AsyncWrite + Unpin + fmt::Debug + 'static,
{
    type Output = Result<Connection<T, SslStream<U>>, io::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        match Pin::new(&mut this.fut).poll(cx) {
            Poll::Ready(Ok(stream)) => {
                let s = this.stream.take().unwrap();
                trace!("SSL Handshake success: {:?}", s.host());
                Poll::Ready(Ok(s.replace(stream).1))
            }
            Poll::Ready(Err(e)) => {
                trace!("SSL Handshake error: {:?}", e);
                Poll::Ready(Err(io::Error::new(io::ErrorKind::Other, format!("{}", e))))
            }
            Poll::Pending => Poll::Pending,
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

impl<T: Address + 'static> ServiceFactory for OpensslConnectServiceFactory<T> {
    type Request = Connect<T>;
    type Response = SslStream<TcpStream>;
    type Error = ConnectError;
    type Config = ();
    type Service = OpensslConnectService<T>;
    type InitError = ();
    type Future = Ready<Result<Self::Service, Self::InitError>>;

    fn new_service(&self, _: ()) -> Self::Future {
        ok(self.service())
    }
}

#[derive(Clone)]
pub struct OpensslConnectService<T> {
    tcp: ConnectService<T>,
    openssl: OpensslConnectorService<T, TcpStream>,
}

impl<T: Address + 'static> Service for OpensslConnectService<T> {
    type Request = Connect<T>;
    type Response = SslStream<TcpStream>;
    type Error = ConnectError;
    type Future = OpensslConnectServiceResponse<T>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Connect<T>) -> Self::Future {
        OpensslConnectServiceResponse {
            fut1: Some(self.tcp.call(req)),
            fut2: None,
            openssl: self.openssl.clone(),
        }
    }
}

pub struct OpensslConnectServiceResponse<T: Address + 'static> {
    fut1: Option<<ConnectService<T> as Service>::Future>,
    fut2: Option<<OpensslConnectorService<T, TcpStream> as Service>::Future>,
    openssl: OpensslConnectorService<T, TcpStream>,
}

impl<T: Address> Future for OpensslConnectServiceResponse<T> {
    type Output = Result<SslStream<TcpStream>, ConnectError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(ref mut fut) = self.fut1 {
            match futures::ready!(Pin::new(fut).poll(cx)) {
                Ok(res) => {
                    let _ = self.fut1.take();
                    self.fut2 = Some(self.openssl.call(res));
                }
                Err(e) => return Poll::Ready(Err(e)),
            }
        }

        if let Some(ref mut fut) = self.fut2 {
            match futures::ready!(Pin::new(fut).poll(cx)) {
                Ok(connect) => Poll::Ready(Ok(connect.into_parts().0)),
                Err(e) => Poll::Ready(Err(ConnectError::Io(io::Error::new(
                    io::ErrorKind::Other,
                    e,
                )))),
            }
        } else {
            Poll::Pending
        }
    }
}
