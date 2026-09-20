use std::{cell::Cell, fmt, io, rc::Rc};

use crate::http::message::CurrentIo;
use crate::http::{Request, Response, ResponseError, body::Body, h1::Codec};
use crate::io::{Filter, Io, IoBoxed, IoRef};

/// A lifecycle event sent to an HTTP/1 control service.
///
/// Return [`Control::ack`] to accept the default action, or use the methods on
/// the individual message type to reject or take ownership of the operation.
pub enum Control<F, Err> {
    /// A transport connection has been accepted.
    Connect(Connection<F>),
    /// A complete request head has been decoded.
    Request(NewRequest),
    /// A request asks to upgrade the HTTP/1 connection.
    Upgrade(Upgrade<F>),
    /// A request contains `Expect: 100-continue`.
    Expect(Expect),
    /// The connection is preparing to stop.
    Disconnect(Reason<Err>),
}

#[derive(Debug)]
/// Reason supplied with an HTTP/1 disconnect notification.
pub enum Reason<Err> {
    /// The HTTP service initiated the disconnect.
    Service(ServiceDisconnect),
    /// The application service returned an error.
    Error(Error<Err>),
    /// HTTP/1 decoding, encoding, or timeout processing failed.
    ProtocolError(ProtocolError),
    /// The peer closed the connection or an I/O error occurred.
    PeerGone(PeerGone),
    /// The keep-alive timer expired.
    KeepAlive(KeepAlive),
}

/// The reason the HTTP service is disconnecting.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ServiceDisconnectReason {
    /// The server is shutting down.
    Shutdown,
    /// The HTTP/1 dispatcher relinquished the connection after an upgrade was
    /// handed to the application or control service.
    UpgradeHandled,
    /// Upgrade handling failed.
    UpgradeFailed,
    /// Expectation handling failed.
    ExpectFailed,
    /// The application dropped an unread request payload, preventing reuse of
    /// the connection.
    PayloadDropped,
}

/// The control service's response to an HTTP/1 lifecycle event.
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
    /// Accepts the event and applies its default action.
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
            Control::Disconnect(msg) => f.debug_tuple("Control::Disconnect").field(msg).finish(),
        }
    }
}

impl<Err: ResponseError> Reason<Err> {
    /// Acknowledges the disconnect notification and stops the connection.
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

/// Notification that a connection has been accepted.
#[derive(Debug)]
pub struct Connection<F> {
    id: usize,
    io: Io<F>,
}

impl<F> Connection<F> {
    #[inline]
    /// Returns the connection identifier.
    pub fn id(&self) -> usize {
        self.id
    }

    #[inline]
    /// Returns the connection I/O object.
    pub fn get_ref(&self) -> &Io<F> {
        &self.io
    }

    #[inline]
    /// Returns mutable access to the connection I/O object.
    pub fn get_mut(&mut self) -> &mut Io<F> {
        &mut self.io
    }

    #[inline]
    /// Accepts the connection and starts HTTP request processing.
    pub fn ack(self) -> ControlAck<F> {
        ControlAck {
            result: ControlResult::Connect(self.io),
        }
    }
}

/// Notification that a complete request head has been received.
#[derive(Debug)]
pub struct NewRequest(Request);

impl NewRequest {
    #[inline]
    /// Returns the HTTP request.
    pub fn get_ref(&self) -> &Request {
        &self.0
    }

    #[inline]
    /// Returns mutable access to the HTTP request.
    pub fn get_mut(&mut self) -> &mut Request {
        &mut self.0
    }

    #[inline]
    /// Accepts the request and continues with expectation handling, upgrade
    /// handling, or the application service as appropriate.
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
    /// Rejects the request with the response generated from `err`.
    pub fn fail<E: ResponseError, F>(self, err: E) -> ControlAck<F> {
        let res: Response = (&err).into();
        let (res, body) = res.into_parts();

        ControlAck {
            result: ControlResult::Response(res, body.into()),
        }
    }

    #[inline]
    /// Rejects the request with a custom response.
    pub fn fail_with<F>(self, res: Response) -> ControlAck<F> {
        let (res, body) = res.into_parts();

        ControlAck {
            result: ControlResult::Response(res, body.into()),
        }
    }
}

/// A request to upgrade the HTTP/1 connection.
pub struct Upgrade<F> {
    req: Request,
    io: Rc<Io<F>>,
    codec: Codec,
}

struct RequestIoAccess<F> {
    io: Rc<Io<F>>,
    codec: Codec,
    taken: Cell<bool>,
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
        if self.taken.get() {
            None
        } else {
            Some(self.io.as_ref())
        }
    }

    fn take(&self) -> Option<(IoBoxed, Codec)> {
        if self.taken.replace(true) {
            None
        } else {
            Some((self.io.take().into(), self.codec.clone()))
        }
    }
}

impl<F: Filter> Upgrade<F> {
    #[inline]
    /// Returns the connection I/O object.
    pub fn io(&self) -> &Io<F> {
        &self.io
    }

    #[inline]
    /// Returns the upgrade request.
    pub fn get_ref(&self) -> &Request {
        &self.req
    }

    #[inline]
    /// Returns mutable access to the upgrade request.
    pub fn get_mut(&mut self) -> &mut Request {
        &mut self.req
    }

    #[inline]
    /// Passes the upgrade request to the application service.
    ///
    /// The application can take ownership of the connection and codec through
    /// [`RequestHead::take_io`](crate::http::RequestHead::take_io).
    pub fn ack(mut self) -> ControlAck<F> {
        // Move io into request
        let io = Rc::new(RequestIoAccess {
            io: self.io,
            codec: self.codec,
            taken: Cell::new(false),
        });
        self.req.head_mut().io = CurrentIo::new(io);

        ControlAck {
            result: ControlResult::UpgradeAck(self.req),
        }
    }

    #[inline]
    /// Takes ownership of the connection, request, and codec.
    ///
    /// Returning this acknowledgement tells the dispatcher that the control
    /// service is responsible for the upgraded connection.
    pub fn handle(self) -> (ControlAck<F>, Io<F>, Request, Codec) {
        (
            ControlAck {
                result: ControlResult::UpgradeHandled,
            },
            self.io.take(),
            self.req,
            self.codec,
        )
    }

    #[inline]
    /// Rejects the upgrade with the response generated from `err`.
    pub fn fail<E: ResponseError>(self, err: E) -> ControlAck<F> {
        let res: Response = (&err).into();
        let (res, body) = res.into_parts();

        ControlAck {
            result: ControlResult::UpgradeFailed(res, body.into()),
        }
    }

    #[inline]
    /// Rejects the upgrade with a custom response.
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

/// Notification that the server is closing the connection.
#[derive(Debug)]
pub struct ServiceDisconnect(ServiceDisconnectReason);

impl ServiceDisconnect {
    fn new(reason: ServiceDisconnectReason) -> Self {
        Self(reason)
    }

    #[inline]
    /// Returns why the connection is being closed.
    pub fn reason(&self) -> ServiceDisconnectReason {
        self.0
    }

    #[inline]
    /// Acknowledges the notification and closes the connection.
    pub fn ack<F>(self) -> ControlAck<F> {
        ControlAck {
            result: ControlResult::Stop,
        }
    }
}

/// Notification that a keep-alive connection is being closed.
#[derive(Debug)]
pub struct KeepAlive {
    enabled: bool,
}

impl KeepAlive {
    pub(super) fn new(enabled: bool) -> Self {
        Self { enabled }
    }

    #[inline]
    /// Returns whether keep-alive was enabled for the connection.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    #[inline]
    /// Acknowledges the notification and closes the connection.
    pub fn ack<F>(self) -> ControlAck<F> {
        ControlAck {
            result: ControlResult::Stop,
        }
    }
}

/// An application service error and its generated response.
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
    /// Returns the application service error.
    pub fn get_ref(&self) -> &Err {
        &self.err
    }

    #[inline]
    /// Returns mutable access to the application service error.
    pub fn get_mut(&mut self) -> &mut Err {
        &mut self.err
    }

    #[inline]
    /// Sends the response generated from the service error.
    pub fn ack<F>(self) -> ControlAck<F> {
        let (res, body) = self.pkt.into_parts();
        ControlAck {
            result: ControlResult::Error(res, body.into()),
        }
    }

    #[inline]
    /// Replaces the generated response with a response produced from `err`.
    pub fn fail<E: ResponseError, F>(self, err: E) -> ControlAck<F> {
        let res: Response = (&err).into();
        let (res, body) = res.into_parts();

        ControlAck {
            result: ControlResult::Error(res, body.into()),
        }
    }

    #[inline]
    /// Replaces the generated response with a custom response.
    pub fn fail_with<F>(self, res: Response) -> ControlAck<F> {
        let (res, body) = res.into_parts();

        ControlAck {
            result: ControlResult::Error(res, body.into()),
        }
    }
}

/// A protocol error reported to the control service.
#[derive(Debug)]
pub struct ProtocolError(super::ProtocolError);

impl ProtocolError {
    #[inline]
    /// Returns the protocol error.
    pub fn get_ref(&self) -> &super::ProtocolError {
        &self.0
    }

    #[inline]
    /// Sends the response generated from the protocol error.
    pub fn ack<F>(self) -> ControlAck<F> {
        let (res, body) = self.0.error_response().into_parts();

        ControlAck {
            result: ControlResult::ProtocolError(res, body.into()),
        }
    }

    #[inline]
    /// Replaces the generated response with a response produced from `err`.
    pub fn fail<E: ResponseError, F>(self, err: E) -> ControlAck<F> {
        let res: Response = (&err).into();
        let (res, body) = res.into_parts();

        ControlAck {
            result: ControlResult::ProtocolError(res, body.into()),
        }
    }

    #[inline]
    /// Replaces the generated response with a custom response.
    pub fn fail_with<F>(self, res: Response) -> ControlAck<F> {
        let (res, body) = res.into_parts();

        ControlAck {
            result: ControlResult::ProtocolError(res, body.into()),
        }
    }
}

/// Notification that the peer closed the connection.
#[derive(Debug)]
pub struct PeerGone(Option<io::Error>);

impl PeerGone {
    #[inline]
    /// Returns the underlying I/O error, if one was reported.
    pub fn get_ref(&self) -> Option<&io::Error> {
        self.0.as_ref()
    }

    #[inline]
    /// Returns mutable access to the underlying I/O error, if one was reported.
    pub fn get_mut(&mut self) -> Option<&mut io::Error> {
        self.0.as_mut()
    }

    #[inline]
    /// Takes the underlying I/O error, if one was reported.
    pub fn take(&mut self) -> Option<io::Error> {
        self.0.take()
    }

    #[inline]
    /// Acknowledges the notification and stops the connection.
    pub fn ack<F>(self) -> ControlAck<F> {
        ControlAck {
            result: ControlResult::Stop,
        }
    }
}

/// A request containing an `Expect: 100-continue` header.
#[derive(Debug)]
pub struct Expect(Request);

impl Expect {
    #[inline]
    /// Returns the HTTP request.
    pub fn get_ref(&self) -> &Request {
        &self.0
    }

    #[inline]
    /// Returns mutable access to the HTTP request.
    pub fn get_mut(&mut self) -> &mut Request {
        &mut self.0
    }

    #[inline]
    /// Sends `100 Continue` and passes the request to the application service.
    pub fn ack<F>(self) -> ControlAck<F> {
        ControlAck {
            result: ControlResult::Continue(self.0),
        }
    }

    #[inline]
    /// Rejects the expectation with the response generated from `err`.
    pub fn fail<E: ResponseError, F>(self, err: E) -> ControlAck<F> {
        let res: Response = (&err).into();
        let (res, body) = res.into_parts();

        ControlAck {
            result: ControlResult::ExpectFailed(res, body.into()),
        }
    }

    #[inline]
    /// Rejects the expectation with a custom response.
    pub fn fail_with<F>(self, res: Response) -> ControlAck<F> {
        let (res, body) = res.into_parts();

        ControlAck {
            result: ControlResult::ExpectFailed(res, body.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::HttpServiceConfig;
    use crate::http::message::IoAccess;
    use crate::service::cfg::SharedCfg;
    use crate::testing::IoTest;

    #[crate::rt_test]
    async fn request_io_access_is_one_shot() {
        let (_, server) = IoTest::create();
        let cfg: SharedCfg = SharedCfg::new("TEST").add(HttpServiceConfig::new()).into();
        let access = RequestIoAccess {
            io: Rc::new(Io::new(server, cfg.clone())),
            codec: Codec::new(1, cfg.get()),
            taken: Cell::new(false),
        };

        assert!(access.get().is_some());
        assert!(access.take().is_some());
        assert!(access.get().is_none());
        assert!(access.take().is_none());
    }
}
