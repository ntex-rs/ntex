//! Tcp connector service
#![deny(rust_2018_idioms, unreachable_pub, missing_debug_implementations)]

mod error;
mod message;
mod resolve;
mod service;
mod uri;

#[cfg(feature = "openssl")]
pub mod openssl;

#[cfg(feature = "rustls")]
pub mod rustls;

pub use self::error::ConnectError;
pub use self::message::{Address, Connect};
pub use self::resolve::Resolver;
pub use self::service::Connector;

use ntex_io::Io;

/// Resolve and connect to remote host
pub async fn connect<T, U>(message: U) -> Result<Io, ConnectError>
where
    T: Address,
    Connect<T>: From<U>,
{
    Connector::new().connect(message).await
}

#[allow(unused_imports)]
#[doc(hidden)]
pub mod net {
    pub use ntex_net::*;
}
