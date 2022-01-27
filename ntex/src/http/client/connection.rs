use std::{cell::RefCell, fmt, rc::Rc, time};

use h2::client::SendRequest;
use ntex_tls::types::HttpProtocol;

use crate::http::body::MessageBody;
use crate::http::message::{RequestHeadType, ResponseHead};
use crate::http::payload::Payload;
use crate::io::IoBoxed;
use crate::util::Bytes;

use super::error::SendRequestError;
use super::pool::Acquired;
use super::{h1proto, h2proto};

pub(super) enum ConnectionType {
    H1(IoBoxed),
    H2(H2Sender),
}

impl fmt::Debug for ConnectionType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConnectionType::H1(_) => write!(f, "http/1"),
            ConnectionType::H2(_) => write!(f, "http/2"),
        }
    }
}

#[derive(Clone)]
pub(super) struct H2Sender(Rc<RefCell<H2SenderInner>>);

struct H2SenderInner {
    io: SendRequest<Bytes>,
    closed: bool,
}

impl H2Sender {
    pub(super) fn new(io: SendRequest<Bytes>) -> Self {
        Self(Rc::new(RefCell::new(H2SenderInner { io, closed: false })))
    }

    pub(super) fn is_closed(&self) -> bool {
        self.0.borrow().closed
    }

    pub(super) fn close(&self) {
        self.0.borrow_mut().closed = true;
    }

    pub(super) fn get_sender(&self) -> SendRequest<Bytes> {
        self.0.borrow().io.clone()
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
        match self.io {
            Some(ConnectionType::H1(_)) => write!(f, "H1Connection"),
            Some(ConnectionType::H2(_)) => write!(f, "H2Connection"),
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

    pub(super) fn release(self) {
        if let Some(mut pool) = self.pool {
            pool.release(Self {
                io: self.io,
                created: self.created,
                pool: None,
            });
        }
    }

    pub(super) fn into_inner(self) -> (ConnectionType, time::Instant) {
        (self.io.unwrap(), self.created)
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
    ) -> Result<(ResponseHead, Payload), SendRequestError> {
        match self.io.take().unwrap() {
            ConnectionType::H1(io) => {
                h1proto::send_request(io, head.into(), body, self.created, self.pool).await
            }
            ConnectionType::H2(io) => h2proto::send_request(io, head.into(), body).await,
        }
    }
}
