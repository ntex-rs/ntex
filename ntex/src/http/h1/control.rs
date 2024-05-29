use std::{future::Future, io};

use crate::http::message::CurrentIo;
use crate::http::{body::Body, h1::Codec, Request, Response, ResponseError};
use crate::io::{Filter, Io, IoBoxed};

#[derive(Debug)]
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
    /// forward request to publish service
    PublishUpgrade(Request),
    /// send response
    Response(Response<()>, Body),
    /// send response
    ResponseWithIo(Response<()>, Body, IoBoxed),
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

    pub(super) fn upgrade(req: Request, io: Io<F>, codec: Codec) -> Self {
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

#[derive(Debug)]
pub struct Upgrade<F> {
    req: Request,
    io: Io<F>,
    codec: Codec,
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
        let io: IoBoxed = self.io.into();
        io.stop_timer();
        self.req.head_mut().io = CurrentIo::new(io, self.codec);

        ControlAck {
            result: ControlResult::PublishUpgrade(self.req),
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
        let _ = crate::rt::spawn(async move {
            let _ = f(self.req, self.io, self.codec).await;
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
            result: ControlResult::ResponseWithIo(res, body.into(), self.io.into()),
            flags: ControlFlags::DISCONNECT,
        }
    }

    #[inline]
    /// Fail request and send custom response
    pub fn fail_with(self, res: Response) -> ControlAck {
        let (res, body) = res.into_parts();

        ControlAck {
            result: ControlResult::ResponseWithIo(res, body.into(), self.io.into()),
            flags: ControlFlags::DISCONNECT,
        }
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
