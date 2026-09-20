//! HTTP/1 protocol services, codecs, payload decoding, and lifecycle control.
//!
//! [`H1Service`] runs an HTTP/1-only server service. [`Codec`] provides
//! low-level request decoding and response encoding, while [`control`] exposes
//! connection, request, expectation, upgrade, and disconnect events.
use std::rc::Rc;

mod codec;
pub(crate) mod decoder;
mod default;
mod dispatcher;
pub(crate) mod encoder;
mod service;

/// Connection lifecycle messages and acknowledgements.
pub mod control;

pub use self::codec::Codec;
pub use self::control::{Control, ControlAck};
pub use self::decoder::{PayloadDecoder, PayloadItem, PayloadType};
pub use self::default::DefaultControlService;
pub use self::service::H1Service;

pub(super) use self::service::handle_io;
use crate::{channel::bstream::Receiver, util::Bytes};

/// A buffered stream of an HTTP/1 request body's decoded bytes.
///
/// Each item is either a body chunk or a [`PayloadError`](super::error::PayloadError).
/// Normal body completion closes the stream, after which receiving returns
/// `None`. A payload error is yielded once before the stream terminates.
///
/// The HTTP/1 dispatcher stops reading body data while this stream's buffer is
/// full. Consuming items therefore releases transport-level backpressure.
/// Dropping the stream before the complete body has been decoded prevents the
/// connection from being reused and causes the dispatcher to disconnect it.
pub type Payload = Receiver<super::error::PayloadError>;

/// A message passed to an HTTP/1 request or response encoder.
///
/// Encoding a message starts with [`Message::Item`]. If the head declares a
/// body, zero or more [`Message::Chunk(Some(_))`](Message::Chunk) values follow,
/// and [`Message::Chunk(None)`](Message::Chunk) completes the body. The final
/// `None` writes the chunked terminator when required and verifies that a
/// fixed-length body supplied all declared bytes.
///
/// A new [`Message::Item`] must not be encoded until the preceding body has
/// completed. Body chunks should be non-empty; use `Message::Chunk(None)` to
/// complete the body explicitly.
#[derive(Debug)]
pub enum Message<T> {
    /// A complete request or response head, including the body-size metadata
    /// required by the concrete codec.
    Item(T),
    /// Body bytes, or `None` to complete the current body.
    Chunk(Option<Bytes>),
}

impl<T> From<T> for Message<T> {
    fn from(item: T) -> Self {
        Message::Item(item)
    }
}

/// Payload state reported by the HTTP client codec.
///
/// Unlike [`PayloadType`], which contains the decoder for an incoming HTTP/1
/// payload, this enum only reports whether the client codec has no payload, a
/// message body, or an upgraded connection stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageType {
    /// The message has no payload.
    None,
    /// The message has a body decoded according to its HTTP framing.
    ///
    /// This includes fixed-length, chunked, and connection-close-delimited
    /// bodies.
    Payload,
    /// The response switched protocols, so subsequent bytes belong to the
    /// upgraded connection rather than an HTTP message body.
    Stream,
}

#[derive(thiserror::Error, Clone, Debug)]
/// Errors that can occur while dispatching HTTP/1 requests.
///
/// The default [`ResponseError`](super::ResponseError) implementation maps
/// header-count and message-head size failures to
/// `431 Request Header Fields Too Large`, other decoding failures to
/// `400 Bad Request`, request and payload timeouts to `408 Request Timeout`,
/// and response encoding or body-stream failures to
/// `500 Internal Server Error`.
pub enum ProtocolError {
    /// HTTP request parsing failed.
    #[error("Parse error: {0}")]
    Decode(#[from] super::error::DecodeError),

    /// HTTP response encoding failed.
    #[error("Encode error: {0}")]
    Encode(#[from] super::error::EncodeError),

    /// The request head did not complete within the configured timeout.
    #[error("Request did not complete within the specified timeout")]
    SlowRequestTimeout,

    /// The request payload did not complete within the configured timeout.
    #[error("Payload did not complete within the specified timeout")]
    SlowPayloadTimeout,

    /// The response body stream returned an error.
    #[error("Response body processing error: {0}")]
    ResponsePayload(Rc<dyn std::error::Error>),
}

impl super::ResponseError for ProtocolError {
    fn error_response(&self) -> super::Response {
        match self {
            ProtocolError::Decode(
                super::error::DecodeError::MaxHeaders | super::error::DecodeError::TooLarge(_),
            ) => super::Response::RequestHeaderFieldsTooLarge().into(),
            ProtocolError::Decode(_) => super::Response::BadRequest().into(),
            ProtocolError::SlowRequestTimeout | ProtocolError::SlowPayloadTimeout => {
                super::Response::RequestTimeout().into()
            }
            ProtocolError::Encode(_) | ProtocolError::ResponsePayload(_) => {
                super::Response::InternalServerError().into()
            }
        }
    }
}
