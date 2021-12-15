//! WebSocket protocol support.
//!
//! To setup a `WebSocket`, first do web socket handshake then on success
//! convert `Payload` into a `WsStream` stream and then use `WsWriter` to
//! communicate with the peer.
use std::io;

use derive_more::{Display, From};

mod codec;
mod frame;
mod mask;
mod proto;
mod sink;
mod stream;

pub use self::codec::{Codec, Frame, Item, Message};
pub use self::frame::Parser;
pub use self::proto::{hash_key, CloseCode, CloseReason, OpCode};
pub use self::sink::WsSink;
pub use self::stream::{StreamDecoder, StreamEncoder};

/// Websocket service errors
#[derive(Debug, Display)]
pub enum WsError<E> {
    Service(E),
    KeepAlive,
    Disconnected,
    Protocol(ProtocolError),
    Io(io::Error),
}

/// Websocket protocol errors
#[derive(Debug, Display, From)]
pub enum ProtocolError {
    /// Received an unmasked frame from client
    #[display(fmt = "Received an unmasked frame from client")]
    UnmaskedFrame,
    /// Received a masked frame from server
    #[display(fmt = "Received a masked frame from server")]
    MaskedFrame,
    /// Encountered invalid opcode
    #[display(fmt = "Invalid opcode: {}", _0)]
    InvalidOpcode(u8),
    /// Invalid control frame length
    #[display(fmt = "Invalid control frame length: {}", _0)]
    InvalidLength(usize),
    /// Bad web socket op code
    #[display(fmt = "Bad web socket op code")]
    BadOpCode,
    /// A payload reached size limit.
    #[display(fmt = "A payload reached size limit.")]
    Overflow,
    /// Continuation is not started
    #[display(fmt = "Continuation is not started.")]
    ContinuationNotStarted,
    /// Received new continuation but it is already started
    #[display(fmt = "Received new continuation but it is already started")]
    ContinuationStarted,
    /// Unknown continuation fragment
    #[display(fmt = "Unknown continuation fragment.")]
    ContinuationFragment(OpCode),
    /// IO Error
    #[display(fmt = "IO Error: {:?}", _0)]
    Io(io::Error),
}

impl std::error::Error for ProtocolError {}
