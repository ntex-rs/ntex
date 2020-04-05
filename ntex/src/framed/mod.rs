mod dispatcher;
mod error;
mod handshake;
mod service;
mod transport;

pub use self::error::ServiceError;
pub use self::handshake::{Handshake, HandshakeResult};
pub use self::service::{Builder, FactoryBuilder};
pub use self::transport::Dispatcher;

#[doc(hidden)]
pub type Connect<T, U> = Handshake<T, U>;
#[doc(hidden)]
pub type ConnectResult<Io, St, Codec, Out> = HandshakeResult<Io, St, Codec, Out>;
