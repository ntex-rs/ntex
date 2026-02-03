use std::{fmt, future::Future, io, rc::Rc};

use crate::http::message::CurrentIo;
use crate::http::{Request, Response, ResponseError, body::Body, h1::Codec};
use crate::io::{Filter, Io, IoBoxed, IoRef};

pub enum Control<F, Err> {
    /// New connection
    Connect(Connection<F>),
    /// New request is loaded
    Request(NewRequest),
    /// Handle `Connection: UPGRADE`
    Upgrade(Upgrade<F>),
    /// Handle `EXPECT` header
    Expect(Expect),
    /// Connection is prepared to disconnect
    Disconnect(Reason<Err>),
}

#[derive(Debug)]
/// Disconnect reason
pub enum Reason<Err> {
    /// Disconnect initiated by service
    Service(ServiceDisconnect),
    /// Application level error
    Error(Error<Err>),
    /// Protocol level error
    ProtocolError(ProtocolError),
    /// Peer is gone
    PeerGone(PeerGone),
    /// Keep-alive timeout
    KeepAlive(KeepAlive),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ServiceDisconnectReason {
    /// Server is shutting down
    Shutdown,
    /// Upgrade request is handled by Upgrade service
    UpgradeHandled,
    /// Upgrade handling failed
    UpgradeFailed,
    /// Expect control message handling failed
    ExpectFailed,
    /// Service is not interested in payload, it is not possible to continue
    PayloadDropped,
}

/// Control message handling result
#[derive(Debug)]
pub struct ControlAck<F> {
    pub(super) result: ControlResult<F>,
}

#[derive(Debug)]
pub(super) enum ControlResult<F> {
    /// Continue
    Connect(Io<F>),
    /// Continue
    Continue(Request),
    /// handle request expect
    Expect(Request),
    /// handle request upgrade
    Upgrade(Request),
    /// upgrade acked
    UpgradeAck(Request),
    /// upgrade handled
    UpgradeHandled,
    /// forward request to publish service
    Publish(Request),
    /// send response
    Response(Response<()>, Body),
    /// service error
    Error(Response<()>, Body),
    /// protocol error
    ProtocolError(Response<()>, Body),
    /// upgrade handling failed
    UpgradeFailed(Response<()>, Body),
    /// expect handling failed
    ExpectFailed(Response<()>, Body),
    /// stop connection
    Stop,
}

impl<F, Err> Control<F, Err> {
    pub(super) fn connect(id: usize, io: Io<F>) -> Self {
        Control::Connect(Connection { id, io })
    }

    pub(super) fn request(req: Request) -> Self {
        Control::Request(NewRequest(req))
    }

    pub(super) fn upgrade(req: Request, io: Rc<Io<F>>, codec: Codec) -> Self {
        Control::Upgrade(Upgrade { req, io, codec })
    }

    pub(super) fn expect(req: Request) -> Self {
        Control::Expect(Expect(req))
    }

    pub(super) fn err(err: Err) -> Self
    where
        Err: ResponseError,
    {
        Control::Disconnect(Reason::Error(Error::new(err)))
    }

    pub(super) fn peer_gone(err: Option<io::Error>) -> Self {
        Control::Disconnect(Reason::PeerGone(PeerGone(err)))
    }

    pub(super) fn proto_err(err: super::ProtocolError) -> Self {
        Control::Disconnect(Reason::ProtocolError(ProtocolError(err)))
    }

    pub(super) fn keepalive(enabled: bool) -> Self {
        Control::Disconnect(Reason::KeepAlive(KeepAlive::new(enabled)))
    }

    pub(super) fn svc_disconnect(reason: ServiceDisconnectReason) -> Self {
        Control::Disconnect(Reason::Service(ServiceDisconnect::new(reason)))
    }

    #[inline]
    /// Ack control message
    pub fn ack(self) -> ControlAck<F>
    where
        F: Filter,
        Err: ResponseError,
    {
        match self {
            Control::Connect(msg) => msg.ack(),
            Control::Request(msg) => msg.ack(),
            Control::Upgrade(msg) => msg.ack(),
            Control::Expect(msg) => msg.ack(),
            Control::Disconnect(msg) => msg.ack(),
        }
    }
}

impl<F, Err> fmt::Debug for Control<F, Err>
where
    Err: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Control::Connect(_) => f.debug_tuple("Control::Connect").finish(),
            Control::Request(msg) => f.debug_tuple("Control::Request").field(msg).finish(),
            Control::Upgrade(msg) => f.debug_tuple("Control::Upgrade").field(msg).finish(),
            Control::Expect(msg) => f.debug_tuple("Control::Expect").field(msg).finish(),
            Control::Disconnect(msg) => {
                f.debug_tuple("Control::Disconnect").field(msg).finish()
            }
        }
    }
}

impl<Err: ResponseError> Reason<Err> {
    pub fn ack<F>(self) -> ControlAck<F> {
        match self {
            Reason::Error(msg) => msg.ack(),
            Reason::ProtocolError(msg) => msg.ack(),
            Reason::PeerGone(msg) => msg.ack(),
            Reason::KeepAlive(msg) => msg.ack(),
            Reason::Service(msg) => msg.ack(),
        }
    }
}

#[derive(Debug)]
pub struct Connection<F> {
    id: usize,
    io: Io<F>,
}

impl<F> Connection<F> {
    #[inline]
    pub fn id(self) -> usize {
        self.id
    }

    #[inline]
    /// Returns reference to Io
    pub fn get_ref(&self) -> &Io<F> {
        &self.io
    }

    #[inline]
    /// Returns mut reference to Io
    pub fn get_mut(&mut self) -> &mut Io<F> {
        &mut self.io
    }

    #[inline]
    /// Ack new request and continue handling process
    pub fn ack(self) -> ControlAck<F> {
        ControlAck {
            result: ControlResult::Connect(self.io),
        }
    }
}

#[derive(Debug)]
pub struct NewRequest(Request);

impl NewRequest {
    #[inline]
    /// Returns reference to http request
    pub fn get_ref(&self) -> &Request {
        &self.0
    }

    #[inline]
    /// Returns mut reference to http request
    pub fn get_mut(&mut self) -> &mut Request {
        &mut self.0
    }

    #[inline]
    /// Ack new request and continue handling process
    pub fn ack<F>(self) -> ControlAck<F> {
        let result = if self.0.head().expect() {
            ControlResult::Expect(self.0)
        } else if self.0.upgrade() {
            ControlResult::Upgrade(self.0)
        } else {
            ControlResult::Publish(self.0)
        };
        ControlAck { result }
    }

    #[inline]
    /// Fail request handling
    pub fn fail<E: ResponseError, F>(self, err: E) -> ControlAck<F> {
        let res: Response = (&err).into();
        let (res, body) = res.into_parts();

        ControlAck {
            result: ControlResult::Response(res, body.into()),
        }
    }

    #[inline]
    /// Fail request and send custom response
    pub fn fail_with<F>(self, res: Response) -> ControlAck<F> {
        let (res, body) = res.into_parts();

        ControlAck {
            result: ControlResult::Response(res, body.into()),
        }
    }
}

pub struct Upgrade<F> {
    req: Request,
    io: Rc<Io<F>>,
    codec: Codec,
}

struct RequestIoAccess<F> {
    io: Rc<Io<F>>,
    codec: Codec,
}

impl<F> fmt::Debug for RequestIoAccess<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RequestIoAccess")
            .field("io", self.io.as_ref())
            .field("codec", &self.codec)
            .finish()
    }
}

impl<F: Filter> crate::http::message::IoAccess for RequestIoAccess<F> {
    fn get(&self) -> Option<&IoRef> {
        Some(self.io.as_ref())
    }

    fn take(&self) -> Option<(IoBoxed, Codec)> {
        Some((self.io.take().into(), self.codec.clone()))
    }
}

impl<F: Filter> Upgrade<F> {
    #[inline]
    /// Returns reference to Io
    pub fn io(&self) -> &Io<F> {
        &self.io
    }

    #[inline]
    /// Returns reference to http request
    pub fn get_ref(&self) -> &Request {
        &self.req
    }

    #[inline]
    /// Returns mut reference to http request
    pub fn get_mut(&mut self) -> &mut Request {
        &mut self.req
    }

    #[inline]
    /// Ack upgrade request and continue handling process
    pub fn ack(mut self) -> ControlAck<F> {
        // Move io into request
        let io = Rc::new(RequestIoAccess {
            io: self.io,
            codec: self.codec,
        });
        self.req.head_mut().io = CurrentIo::new(io);

        ControlAck {
            result: ControlResult::UpgradeAck(self.req),
        }
    }

    #[inline]
    /// Handle upgrade request
    pub fn handle<H, R, O>(self, f: H) -> ControlAck<F>
    where
        H: FnOnce(Request, Io<F>, Codec) -> R + 'static,
        R: Future<Output = O>,
    {
        let io = self.io.take();
        let _ = crate::rt::spawn(async move {
            let _ = f(self.req, io, self.codec).await;
        });
        ControlAck {
            result: ControlResult::UpgradeHandled,
        }
    }

    #[inline]
    /// Fail request handling
    pub fn fail<E: ResponseError>(self, err: E) -> ControlAck<F> {
        let res: Response = (&err).into();
        let (res, body) = res.into_parts();

        ControlAck {
            result: ControlResult::UpgradeFailed(res, body.into()),
        }
    }

    #[inline]
    /// Fail request and send custom response
    pub fn fail_with(self, res: Response) -> ControlAck<F> {
        let (res, body) = res.into_parts();

        ControlAck {
            result: ControlResult::UpgradeFailed(res, body.into()),
        }
    }
}

impl<F> fmt::Debug for Upgrade<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Upgrade")
            .field("req", &self.req)
            .field("io", &self.io)
            .field("codec", &self.codec)
            .finish()
    }
}

/// Service disconnect initiated by server
#[derive(Debug)]
pub struct ServiceDisconnect(ServiceDisconnectReason);

impl ServiceDisconnect {
    fn new(reason: ServiceDisconnectReason) -> Self {
        Self(reason)
    }

    #[inline]
    /// Service disconnect reason
    pub fn reason(&self) -> ServiceDisconnectReason {
        self.0
    }

    #[inline]
    /// Ack controk message
    pub fn ack<F>(self) -> ControlAck<F> {
        ControlAck {
            result: ControlResult::Stop,
        }
    }
}

/// KeepAlive
#[derive(Debug)]
pub struct KeepAlive {
    enabled: bool,
}

impl KeepAlive {
    pub(super) fn new(enabled: bool) -> Self {
        Self { enabled }
    }

    #[inline]
    /// Connection keep-alive is enabled
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    #[inline]
    /// Ack controk message
    pub fn ack<F>(self) -> ControlAck<F> {
        ControlAck {
            result: ControlResult::Stop,
        }
    }
}

/// Service level error
#[derive(Debug)]
pub struct Error<Err> {
    err: Err,
    pkt: Response,
}

impl<Err: ResponseError> Error<Err> {
    fn new(err: Err) -> Self {
        Self {
            pkt: err.error_response(),
            err,
        }
    }

    #[inline]
    /// Returns reference to http error
    pub fn get_ref(&self) -> &Err {
        &self.err
    }

    #[inline]
    /// Ack service error and close connection.
    pub fn ack<F>(self) -> ControlAck<F> {
        let (res, body) = self.pkt.into_parts();
        ControlAck {
            result: ControlResult::Error(res, body.into()),
        }
    }

    #[inline]
    /// Fail error handling
    pub fn fail<E: ResponseError, F>(self, err: E) -> ControlAck<F> {
        let res: Response = (&err).into();
        let (res, body) = res.into_parts();

        ControlAck {
            result: ControlResult::Error(res, body.into()),
        }
    }

    #[inline]
    /// Fail error handling
    pub fn fail_with<F>(self, res: Response) -> ControlAck<F> {
        let (res, body) = res.into_parts();

        ControlAck {
            result: ControlResult::Error(res, body.into()),
        }
    }
}

#[derive(Debug)]
pub struct ProtocolError(super::ProtocolError);

impl ProtocolError {
    #[inline]
    /// Returns error reference
    pub fn err(&self) -> &super::ProtocolError {
        &self.0
    }

    #[inline]
    /// Ack `ProtocolError` message
    pub fn ack<F>(self) -> ControlAck<F> {
        let (res, body) = self.0.error_response().into_parts();

        ControlAck {
            result: ControlResult::ProtocolError(res, body.into()),
        }
    }

    #[inline]
    /// Fail error handling
    pub fn fail<E: ResponseError, F>(self, err: E) -> ControlAck<F> {
        let res: Response = (&err).into();
        let (res, body) = res.into_parts();

        ControlAck {
            result: ControlResult::ProtocolError(res, body.into()),
        }
    }

    #[inline]
    /// Fail error handling
    pub fn fail_with<F>(self, res: Response) -> ControlAck<F> {
        let (res, body) = res.into_parts();

        ControlAck {
            result: ControlResult::ProtocolError(res, body.into()),
        }
    }
}

#[derive(Debug)]
pub struct PeerGone(Option<io::Error>);

impl PeerGone {
    #[inline]
    /// Returns error reference
    pub fn err(&self) -> Option<&io::Error> {
        self.0.as_ref()
    }

    #[inline]
    /// Take error
    pub fn take(&mut self) -> Option<io::Error> {
        self.0.take()
    }

    #[inline]
    /// Ack `PeerGone` message
    pub fn ack<F>(self) -> ControlAck<F> {
        ControlAck {
            result: ControlResult::Stop,
        }
    }
}

#[derive(Debug)]
pub struct Expect(Request);

impl Expect {
    #[inline]
    /// Returns reference to http request
    pub fn get_ref(&self) -> &Request {
        &self.0
    }

    #[inline]
    /// Ack expect request
    pub fn ack<F>(self) -> ControlAck<F> {
        ControlAck {
            result: ControlResult::Continue(self.0),
        }
    }

    #[inline]
    /// Fail expect request
    pub fn fail<E: ResponseError, F>(self, err: E) -> ControlAck<F> {
        let res: Response = (&err).into();
        let (res, body) = res.into_parts();

        ControlAck {
            result: ControlResult::ExpectFailed(res, body.into()),
        }
    }

    #[inline]
    /// Fail expect request and send custom response
    pub fn fail_with<F>(self, res: Response) -> ControlAck<F> {
        let (res, body) = res.into_parts();

        ControlAck {
            result: ControlResult::ExpectFailed(res, body.into()),
        }
    }
}
