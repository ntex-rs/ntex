//! HTTP/1 implementation
use std::rc::Rc;

mod client;
mod codec;
mod decoder;
mod default;
mod dispatcher;
mod encoder;
mod service;

pub mod control;

pub use self::client::{ClientCodec, ClientPayloadCodec};
pub use self::codec::Codec;
pub use self::control::{Control, ControlAck};
pub use self::decoder::{PayloadDecoder, PayloadItem, PayloadType};
pub use self::default::DefaultControlService;
pub use self::service::{H1Service, H1ServiceHandler};

pub(super) use self::dispatcher::Dispatcher;
use crate::{channel::bstream::Receiver, util::Bytes};

pub type Payload = Receiver<super::error::PayloadError>;

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

#[derive(thiserror::Error, Clone, Debug)]
/// A set of errors that can occur during dispatching http requests
pub enum ProtocolError {
    /// Http request parse error.
    #[error("Parse error: {0}")]
    Decode(#[from] super::error::DecodeError),

    /// Http response encoding error.
    #[error("Encode error: {0}")]
    Encode(#[from] super::error::EncodeError),

    /// Request did not complete within the specified timeout
    #[error("Request did not complete within the specified timeout")]
    SlowRequestTimeout,

    /// Payload did not complete within the specified timeout
    #[error("Payload did not complete within the specified timeout")]
    SlowPayloadTimeout,

    /// Response body processing error
    #[error("Response body processing error: {0}")]
    ResponsePayload(Rc<dyn std::error::Error>),
}

impl super::ResponseError for ProtocolError {
    fn error_response(&self) -> super::Response {
        match self {
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
