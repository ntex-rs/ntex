use std::{fmt, future::Future, pin::Pin, time};

use bytes::Bytes;
use h2::client::SendRequest;

use crate::codec::{AsyncRead, AsyncWrite, Framed};
use crate::http::body::MessageBody;
use crate::http::h1::ClientCodec;
use crate::http::message::{RequestHeadType, ResponseHead};
use crate::http::payload::Payload;
use crate::http::Protocol;
use crate::util::{Either, Ready};

use super::error::SendRequestError;
use super::pool::Acquired;
use super::{h1proto, h2proto};

pub(super) enum ConnectionType<Io> {
    H1(Io),
    H2(SendRequest<Bytes>),
}

pub trait Connection {
    type Io: AsyncRead + AsyncWrite + Unpin;
    type Future: Future<Output = Result<(ResponseHead, Payload), SendRequestError>>;

    fn protocol(&self) -> Protocol;

    /// Send request and body
    fn send_request<B: MessageBody + 'static, H: Into<RequestHeadType>>(
        self,
        head: H,
        body: B,
    ) -> Self::Future;

    type TunnelFuture: Future<
        Output = Result<(ResponseHead, Framed<Self::Io, ClientCodec>), SendRequestError>,
    >;

    /// Send request, returns Response and Framed
    fn open_tunnel<H: Into<RequestHeadType>>(self, head: H) -> Self::TunnelFuture;
}

pub(super) trait ConnectionLifetime:
    AsyncRead + AsyncWrite + Unpin + 'static
{
    /// Close connection
    fn close(&mut self);

    /// Release connection to the connection pool
    fn release(&mut self);
}

#[doc(hidden)]
/// HTTP client connection
pub(super) struct IoConnection<T> {
    io: Option<ConnectionType<T>>,
    created: time::Instant,
    pool: Option<Acquired<T>>,
}

impl<T> fmt::Debug for IoConnection<T>
where
    T: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.io {
            Some(ConnectionType::H1(ref io)) => write!(f, "H1Connection({:?})", io),
            Some(ConnectionType::H2(_)) => write!(f, "H2Connection"),
            None => write!(f, "Connection(Empty)"),
        }
    }
}

impl<T> IoConnection<T>
where
    T: AsyncRead + AsyncWrite + Unpin + 'static,
{
    pub(super) fn new(
        io: ConnectionType<T>,
        created: time::Instant,
        pool: Option<Acquired<T>>,
    ) -> Self {
        IoConnection {
            pool,
            created,
            io: Some(io),
        }
    }

    pub(super) fn release(self) {
        if let Some(mut pool) = self.pool {
            pool.release(Self {
                io: self.io,
                created: self.created,
                pool: None,
            });
        }
    }

    pub(super) fn into_inner(self) -> (ConnectionType<T>, time::Instant) {
        (self.io.unwrap(), self.created)
    }
}

impl<T> Connection for IoConnection<T>
where
    T: AsyncRead + AsyncWrite + Unpin + 'static,
{
    type Io = T;
    type Future =
        Pin<Box<dyn Future<Output = Result<(ResponseHead, Payload), SendRequestError>>>>;

    fn protocol(&self) -> Protocol {
        match self.io {
            Some(ConnectionType::H1(_)) => Protocol::Http1,
            Some(ConnectionType::H2(_)) => Protocol::Http2,
            None => Protocol::Http1,
        }
    }

    fn send_request<B: MessageBody + 'static, H: Into<RequestHeadType>>(
        mut self,
        head: H,
        body: B,
    ) -> Self::Future {
        match self.io.take().unwrap() {
            ConnectionType::H1(io) => Box::pin(h1proto::send_request(
                io,
                head.into(),
                body,
                self.created,
                self.pool,
            )),
            ConnectionType::H2(io) => Box::pin(h2proto::send_request(
                io,
                head.into(),
                body,
                self.created,
                self.pool,
            )),
        }
    }

    type TunnelFuture = Either<
        Pin<
            Box<
                dyn Future<
                    Output = Result<
                        (ResponseHead, Framed<Self::Io, ClientCodec>),
                        SendRequestError,
                    >,
                >,
            >,
        >,
        Ready<(ResponseHead, Framed<Self::Io, ClientCodec>), SendRequestError>,
    >;

    /// Send request, returns Response and Framed
    fn open_tunnel<H: Into<RequestHeadType>>(mut self, head: H) -> Self::TunnelFuture {
        match self.io.take().unwrap() {
            ConnectionType::H1(io) => {
                Either::Left(Box::pin(h1proto::open_tunnel(io, head.into())))
            }
            ConnectionType::H2(io) => {
                if let Some(mut pool) = self.pool.take() {
                    pool.release(IoConnection::new(
                        ConnectionType::H2(io),
                        self.created,
                        None,
                    ));
                }
                Either::Right(Ready::Err(SendRequestError::TunnelNotSupported))
            }
        }
    }
}
