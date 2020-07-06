mod handshake;
mod service;

pub use self::handshake::{Handshake, HandshakeResult};
pub use self::service::{Builder, FactoryBuilder};
pub use crate::util::framed::DispatcherError as ServiceError;

pub type Connect<T, U> = Handshake<T, U>;
pub type ConnectResult<Io, St, Codec, Out> = HandshakeResult<Io, St, Codec, Out>;
