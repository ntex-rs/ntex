mod handshake;
mod service;

pub use self::handshake::{Handshake, HandshakeResult};
pub use self::service::{Builder, FactoryBuilder};
pub use crate::util::framed::DispatcherError as ServiceError;

#[doc(hidden)]
#[deprecated(since = "0.1.10", note = "Use Handshake instead")]
pub type Connect<T, U> = Handshake<T, U>;
#[doc(hidden)]
#[deprecated(since = "0.1.10", note = "Use HandshakeResult instead")]
pub type ConnectResult<Io, St, Codec, Out> = HandshakeResult<Io, St, Codec, Out>;
