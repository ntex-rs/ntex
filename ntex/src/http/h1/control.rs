use std::{fmt, future::Future, io, rc::Rc};

use crate::http::message::CurrentIo;
use crate::http::{Request, Response, ResponseError, body::Body, h1::Codec};
use crate::io::{Filter, Io, IoBoxed, IoRef};

pub enum Control<F, Err> {
    /// New request is loaded
    NewRequest(NewRequest),
    /// Handle `Connection: UPGRADE`
    Upgrade(Upgrade<F>),
    /// Handle `EXPECT` header
    Expect(Expect),
    /// Underlying transport connection closed
    Closed(Closed),
    /// Application level error
    Error(Error<Err>),
    /// Protocol level error
    ProtocolError(ProtocolError),
    /// Peer is gone
    PeerGone(PeerGone),
}

/// Control message handling result
#[derive(Debug)]
pub struct ControlAck {
    pub(super) result: ControlResult,
    pub(super) flags: ControlFlags,
}

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
    pub(super) struct ControlFlags: u8 {
        /// Disconnect after request handling
        const DISCONNECT = 0b0000_0001;
        /// Handle expect-continue
        const CONTINUE   = 0b0001_0000;
    }
}

#[derive(Debug)]
pub(super) enum ControlResult {
    /// handle request expect
    Expect(Request),
    /// handle request upgrade
    Upgrade(Request),
    /// forward request to publish service
    Publish(Request),
    /// send response
    Response(Response<()>, Body),
    /// drop connection
    Stop,
}

impl<F, Err> Control<F, Err> {
    pub(super) fn err(err: Err) -> Self
    where
        Err: ResponseError,
    {
        Control::Error(Error::new(err))
    }

    pub(super) const fn closed() -> Self {
        Control::Closed(Closed)
    }

    pub(super) fn new_req(req: Request) -> Self {
        Control::NewRequest(NewRequest(req))
    }

    pub(super) fn upgrade(req: Request, io: Rc<Io<F>>, codec: Codec) -> Self {
        Control::Upgrade(Upgrade { req, io, codec })
    }

    pub(super) fn expect(req: Request) -> Self {
        Control::Expect(Expect(req))
    }

    pub(super) fn peer_gone(err: Option<io::Error>) -> Self {
        Control::PeerGone(PeerGone(err))
    }

    pub(super) fn proto_err(err: super::ProtocolError) -> Self {
        Control::ProtocolError(ProtocolError(err))
    }

    #[inline]
    /// Ack control message
    pub fn ack(self) -> ControlAck
    where
        F: Filter,
        Err: ResponseError,
    {
        match self {
            Control::NewRequest(msg) => msg.ack(),
            Control::Upgrade(msg) => msg.ack(),
            Control::Expect(msg) => msg.ack(),
            Control::Closed(msg) => msg.ack(),
            Control::Error(msg) => msg.ack(),
            Control::ProtocolError(msg) => msg.ack(),
            Control::PeerGone(msg) => msg.ack(),
        }
    }
}

impl<F, Err> fmt::Debug for Control<F, Err>
where
    Err: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Control::NewRequest(msg) => {
                f.debug_tuple("Control::NewRequest").field(msg).finish()
            }
            Control::Upgrade(msg) => f.debug_tuple("Control::Upgrade").field(msg).finish(),
            Control::Expect(msg) => f.debug_tuple("Control::Expect").field(msg).finish(),
            Control::Closed(msg) => f.debug_tuple("Control::Closed").field(msg).finish(),
            Control::Error(msg) => f.debug_tuple("Control::Error").field(msg).finish(),
            Control::ProtocolError(msg) => {
                f.debug_tuple("Control::ProtocolError").field(msg).finish()
            }
            Control::PeerGone(msg) => {
                f.debug_tuple("Control::PeerGone").field(msg).finish()
            }
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
    pub fn ack(self) -> ControlAck {
        let result = if self.0.head().expect() {
            ControlResult::Expect(self.0)
        } else if self.0.upgrade() {
            ControlResult::Upgrade(self.0)
        } else {
            ControlResult::Publish(self.0)
        };
        ControlAck {
            result,
            flags: ControlFlags::empty(),
        }
    }

    #[inline]
    /// Fail request handling
    pub fn fail<E: ResponseError>(self, err: E) -> ControlAck {
        let res: Response = (&err).into();
        let (res, body) = res.into_parts();

        ControlAck {
            result: ControlResult::Response(res, body.into()),
            flags: ControlFlags::empty(),
        }
    }

    #[inline]
    /// Fail request and send custom response
    pub fn fail_with(self, res: Response) -> ControlAck {
        let (res, body) = res.into_parts();

        ControlAck {
            result: ControlResult::Response(res, body.into()),
            flags: ControlFlags::empty(),
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
    pub fn ack(mut self) -> ControlAck {
        // Move io into request
        let io = Rc::new(RequestIoAccess {
            io: self.io,
            codec: self.codec,
        });
        self.req.head_mut().io = CurrentIo::new(io);

        ControlAck {
            result: ControlResult::Publish(self.req),
            flags: ControlFlags::DISCONNECT,
        }
    }

    #[inline]
    /// Handle upgrade request
    pub fn handle<H, R, O>(self, f: H) -> ControlAck
    where
        H: FnOnce(Request, Io<F>, Codec) -> R + 'static,
        R: Future<Output = O>,
    {
        let io = self.io.take();
        let _ = crate::rt::spawn(async move {
            let _ = f(self.req, io, self.codec).await;
        });
        ControlAck {
            result: ControlResult::Stop,
            flags: ControlFlags::DISCONNECT,
        }
    }

    #[inline]
    /// Fail request handling
    pub fn fail<E: ResponseError>(self, err: E) -> ControlAck {
        let res: Response = (&err).into();
        let (res, body) = res.into_parts();

        ControlAck {
            result: ControlResult::Response(res, body.into()),
            flags: ControlFlags::DISCONNECT,
        }
    }

    #[inline]
    /// Fail request and send custom response
    pub fn fail_with(self, res: Response) -> ControlAck {
        let (res, body) = res.into_parts();

        ControlAck {
            result: ControlResult::Response(res, body.into()),
            flags: ControlFlags::DISCONNECT,
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

/// Connection closed message
#[derive(Debug)]
pub struct Closed;

impl Closed {
    #[inline]
    /// convert packet to a result
    pub fn ack(self) -> ControlAck {
        ControlAck {
            result: ControlResult::Stop,
            flags: ControlFlags::empty(),
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
    pub fn ack(self) -> ControlAck {
        let (res, body) = self.pkt.into_parts();
        ControlAck {
            result: ControlResult::Response(res, body.into()),
            flags: ControlFlags::DISCONNECT,
        }
    }

    #[inline]
    /// Fail error handling
    pub fn fail<E: ResponseError>(self, err: E) -> ControlAck {
        let res: Response = (&err).into();
        let (res, body) = res.into_parts();

        ControlAck {
            result: ControlResult::Response(res, body.into()),
            flags: ControlFlags::DISCONNECT,
        }
    }

    #[inline]
    /// Fail error handling
    pub fn fail_with(self, res: Response) -> ControlAck {
        let (res, body) = res.into_parts();

        ControlAck {
            result: ControlResult::Response(res, body.into()),
            flags: ControlFlags::DISCONNECT,
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
    /// Ack ProtocolError message
    pub fn ack(self) -> ControlAck {
        let (res, body) = self.0.error_response().into_parts();

        ControlAck {
            result: ControlResult::Response(res, body.into()),
            flags: ControlFlags::DISCONNECT,
        }
    }

    #[inline]
    /// Fail error handling
    pub fn fail<E: ResponseError>(self, err: E) -> ControlAck {
        let res: Response = (&err).into();
        let (res, body) = res.into_parts();

        ControlAck {
            result: ControlResult::Response(res, body.into()),
            flags: ControlFlags::DISCONNECT,
        }
    }

    #[inline]
    /// Fail error handling
    pub fn fail_with(self, res: Response) -> ControlAck {
        let (res, body) = res.into_parts();

        ControlAck {
            result: ControlResult::Response(res, body.into()),
            flags: ControlFlags::DISCONNECT,
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
    /// Ack PeerGone message
    pub fn ack(self) -> ControlAck {
        ControlAck {
            result: ControlResult::Stop,
            flags: ControlFlags::DISCONNECT,
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
    pub fn ack(self) -> ControlAck {
        let result = if self.0.upgrade() {
            ControlResult::Upgrade(self.0)
        } else {
            ControlResult::Publish(self.0)
        };
        ControlAck {
            result,
            flags: ControlFlags::CONTINUE,
        }
    }

    #[inline]
    /// Fail expect request
    pub fn fail<E: ResponseError>(self, err: E) -> ControlAck {
        let res: Response = (&err).into();
        let (res, body) = res.into_parts();

        ControlAck {
            result: ControlResult::Response(res, body.into()),
            flags: ControlFlags::DISCONNECT,
        }
    }

    #[inline]
    /// Fail expect request and send custom response
    pub fn fail_with(self, res: Response) -> ControlAck {
        let (res, body) = res.into_parts();

        ControlAck {
            result: ControlResult::Response(res, body.into()),
            flags: ControlFlags::DISCONNECT,
        }
    }
}
