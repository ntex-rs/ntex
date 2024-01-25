//! HTTP/1 implementation
use crate::util::{Bytes, BytesMut};

mod client;
mod codec;
mod decoder;
mod dispatcher;
mod encoder;
mod expect;
mod payload;
mod service;
mod upgrade;

pub mod control;

pub use self::client::{ClientCodec, ClientPayloadCodec};
pub use self::codec::Codec;
pub use self::decoder::{PayloadDecoder, PayloadItem, PayloadType};
pub use self::expect::ExpectHandler;
pub use self::payload::Payload;
pub use self::service::{H1Service, H1ServiceHandler};
pub use self::upgrade::UpgradeHandler;

pub(super) use self::dispatcher::Dispatcher;

const MAX_BUFFER_SIZE: usize = 32_768;

#[derive(Debug)]
/// Codec message
pub enum Message<T> {
    /// Http message
    Item(T),
    /// Payload chunk
    Chunk(Option<Bytes>),
}

impl<T> From<T> for Message<T> {
    fn from(item: T) -> Self {
        Message::Item(item)
    }
}

/// Incoming request type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageType {
    None,
    Payload,
    Stream,
}

const LW: usize = 2 * 1024;
const HW: usize = 32 * 1024;

pub(crate) fn reserve_readbuf(src: &mut BytesMut) {
    let cap = src.capacity();
    if cap < LW {
        src.reserve(HW - cap);
    }
}

#[derive(thiserror::Error, Debug)]
/// A set of errors that can occur during dispatching http requests
pub enum ProtocolError {
    /// Http request parse error.
    #[error("Parse error: {0}")]
    Decode(#[from] super::error::DecodeError),

    /// Http response encoding error.
    #[error("Encode error: {0}")]
    Encode(std::io::Error),

    /// Request did not complete within the specified timeout
    #[error("Request did not complete within the specified timeout")]
    SlowRequestTimeout,

    /// Payload did not complete within the specified timeout
    #[error("Payload did not complete within the specified timeout")]
    SlowPayloadTimeout,

    /// Disconnect timeout. Makes sense for ssl streams.
    #[error("Connection shutdown timeout")]
    DisconnectTimeout,

    /// Payload is not consumed
    #[error("Task is completed but request's payload is not consumed")]
    PayloadIsNotConsumed,

    /// Malformed request
    #[error("Malformed request")]
    MalformedRequest,

    /// Response body processing error
    #[error("Response body processing error: {0}")]
    ResponsePayload(Box<dyn std::error::Error>),

    /// Internal error
    #[error("Internal error")]
    InternalError,

    /// Unknown error
    #[error("Unknown error")]
    Unknown,
}
