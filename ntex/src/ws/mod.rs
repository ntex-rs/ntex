//! WebSocket protocol support.
//!
//! To setup a `WebSocket`, first do web socket handshake then on success
//! convert `Payload` into a `WsStream` stream and then use `WsWriter` to
//! communicate with the peer.
mod client;
mod codec;
mod frame;
mod handshake;
mod mask;
mod proto;
mod sink;
mod transport;

pub mod error;

pub use self::client::{WsClient, WsClientBuilder, WsConnection};
pub use self::codec::{Codec, Frame, Item, Message};
pub use self::frame::Parser;
pub use self::handshake::{handshake, handshake_response, verify_handshake};
pub use self::proto::{hash_key, CloseCode, CloseReason, OpCode};
pub use self::sink::WsSink;
pub use self::transport::{WsTransport, WsTransportService};
