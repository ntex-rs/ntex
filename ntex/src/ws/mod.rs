//! WebSocket protocol support.
//!
//! Use [`handshake()`] or [`handshake_response()`] to perform a server-side
//! opening handshake. For framed WebSocket communication, use [`WsTransport`]
//! with [`WsSink`], or use [`WsClient`] to establish a client connection.
mod cfg;
mod client;
mod codec;
mod frame;
mod handshake;
mod mask;
mod proto;
mod sink;
mod transport;

pub mod error;

pub use self::cfg::WsClientConfig;
pub use self::client::{WsClient, WsConnection};
pub use self::codec::{Codec, Frame, Item, Message};
pub use self::frame::Parser;
pub use self::handshake::{handshake, handshake_response, verify_handshake};
pub use self::proto::{CloseCode, CloseReason, OpCode, hash_key};
pub use self::sink::WsSink;
pub use self::transport::{WsTransport, WsTransportService};

pub(crate) use self::cfg::is_token;
