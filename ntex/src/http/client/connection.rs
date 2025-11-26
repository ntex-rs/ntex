use std::{fmt, time};

use crate::http::body::MessageBody;
use crate::http::message::{RequestHeadType, ResponseHead};
use crate::http::payload::Payload;
use crate::io::{IoBoxed, types::HttpProtocol};
use crate::time::Millis;

use super::{error::SendRequestError, h1proto, h2proto, pool::Acquired};

pub(super) enum ConnectionType {
    H1(IoBoxed),
    H2(h2proto::H2Client),
}

impl ConnectionType {
    pub(super) fn tag(&self) -> &'static str {
        match &self {
            ConnectionType::H1(io) => io.tag(),
            ConnectionType::H2(io) => io.tag(),
        }
    }
}

impl fmt::Debug for ConnectionType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConnectionType::H1(_) => write!(f, "http/1"),
            ConnectionType::H2(_) => write!(f, "http/2"),
        }
    }
}

#[doc(hidden)]
/// HTTP client connection
pub struct Connection {
    io: Option<ConnectionType>,
    created: time::Instant,
    pool: Option<Acquired>,
}

impl fmt::Debug for Connection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.io {
            Some(ConnectionType::H1(io)) => write!(f, "{}: Connection(h1)", io.tag()),
            Some(ConnectionType::H2(io)) => write!(f, "{}: Connection(h2)", io.tag()),
            None => write!(f, "Connection(Empty)"),
        }
    }
}

impl Connection {
    pub(super) fn new(
        io: ConnectionType,
        created: time::Instant,
        pool: Option<Acquired>,
    ) -> Self {
        Self {
            pool,
            created,
            io: Some(io),
        }
    }

    pub(super) fn release(self, close: bool) {
        if let Some(mut pool) = self.pool {
            pool.release(
                Self {
                    io: self.io,
                    created: self.created,
                    pool: None,
                },
                close,
            );
        } else {
            log::debug!("{:?}: http pool is not set, dropping", self);
        }
    }

    pub(super) fn into_inner(self) -> (ConnectionType, time::Instant, Option<Acquired>) {
        (self.io.unwrap(), self.created, self.pool)
    }

    pub fn protocol(&self) -> HttpProtocol {
        match self.io {
            Some(ConnectionType::H1(_)) => HttpProtocol::Http1,
            Some(ConnectionType::H2(_)) => HttpProtocol::Http2,
            None => HttpProtocol::Unknown,
        }
    }

    pub(super) async fn send_request<B: MessageBody + 'static, H: Into<RequestHeadType>>(
        mut self,
        head: H,
        body: B,
        timeout: Millis,
    ) -> Result<(ResponseHead, Payload), SendRequestError> {
        match self.io.take().unwrap() {
            ConnectionType::H1(io) => {
                h1proto::send_request(
                    io,
                    head.into(),
                    body,
                    self.created,
                    timeout,
                    self.pool,
                )
                .await
            }
            ConnectionType::H2(io) => {
                h2proto::send_request(io, head.into(), body, timeout).await
            }
        }
    }
}
