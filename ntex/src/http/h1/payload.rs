use std::task::{Context, Poll};
use std::{cell::RefCell, ops::Deref, pin::Pin, rc::Rc};

use crate::channel::bstream::{self, Receiver};
use crate::http::{HeaderMap, error::PayloadError};
use crate::util::{Bytes, Stream};

/// A buffered stream of an HTTP/1 request body's decoded bytes.
///
/// Each item is either a body chunk or a [`PayloadError`].
/// Normal body completion closes the stream, after which receiving returns
/// `None`. A payload error is yielded once before the stream terminates.
///
/// The HTTP/1 dispatcher stops reading body data while this stream's buffer is
/// full. Consuming items therefore releases transport-level backpressure.
/// Dropping the stream before the complete body has been decoded prevents the
/// connection from being reused and causes the dispatcher to disconnect it.
///
/// Dereferences to the underlying [`bstream::Receiver`].
#[derive(Debug)]
pub struct Payload {
    rx: Receiver<PayloadError>,
    trailers: Rc<RefCell<Option<HeaderMap>>>,
}

impl Payload {
    /// Creates a payload stream and its sender.
    pub(crate) fn create() -> (PayloadSender, Payload) {
        let (tx, rx) = bstream::channel();
        let trailers = Rc::new(RefCell::new(None));
        (
            PayloadSender {
                tx,
                trailers: trailers.clone(),
            },
            Payload { rx, trailers },
        )
    }

    /// Returns the trailer fields of a chunked payload.
    ///
    /// Trailers are available once the payload is complete, `None` is
    /// returned if the payload is not complete or has no trailers.
    pub fn trailers(&self) -> Option<HeaderMap> {
        self.trailers.borrow().clone()
    }
}

impl From<Receiver<PayloadError>> for Payload {
    fn from(rx: Receiver<PayloadError>) -> Self {
        Payload {
            rx,
            trailers: Rc::default(),
        }
    }
}

impl Deref for Payload {
    type Target = Receiver<PayloadError>;

    fn deref(&self) -> &Self::Target {
        &self.rx
    }
}

impl Stream for Payload {
    type Item = Result<Bytes, PayloadError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_read(cx)
    }
}

/// Sender side of the HTTP/1 payload stream.
#[derive(Debug)]
pub(crate) struct PayloadSender {
    tx: bstream::Sender<PayloadError>,
    trailers: Rc<RefCell<Option<HeaderMap>>>,
}

impl PayloadSender {
    /// Stores the trailer fields, the payload is completed with `feed_eof()`.
    pub(crate) fn feed_trailers(&self, trailers: HeaderMap) {
        *self.trailers.borrow_mut() = Some(trailers);
    }
}

impl Deref for PayloadSender {
    type Target = bstream::Sender<PayloadError>;

    fn deref(&self) -> &Self::Target {
        &self.tx
    }
}
