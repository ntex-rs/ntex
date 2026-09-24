//! WebSocket protocol support.
//!
//! Use [`handshake()`] or [`handshake_response()`] to perform a server-side
//! opening handshake, and [`WsClient`] to establish a client connection.
//!
//! Framed communication uses [`Codec`] to encode and decode [`Frame`]s and
//! [`Message`]s. On the client side, [`WsConnection::start()`] runs a frame
//! handling service, [`WsConnection::receiver()`] returns a stream of frames,
//! and [`WsConnection::sink()`] returns a [`WsSink`] for sending messages.
//!
//! [`WsTransport`] is a byte-stream filter instead: binary and continuation
//! frames are exposed as raw bytes, text frames are rejected, and control
//! frames are handled internally. Use [`WsConnection::into_transport()`] to
//! run a byte-oriented protocol over a WebSocket connection.
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
