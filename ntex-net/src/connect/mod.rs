//! Tcp connector service
mod error;
mod message;
mod resolve;
mod service;
mod uri;

pub use self::error::ConnectError;
pub use self::message::{Address, Connect};
pub use self::resolve::Resolver;
pub use self::service::Connector;

use ntex_io::{Io, IoConfig};

/// Resolve and connect to remote host
pub async fn connect<T, U>(message: U) -> Result<Io, ConnectError>
where
    T: Address,
    Connect<T>: From<U>,
{
    Connector::new().connect(message).await
}

/// Resolve and connect to remote host
pub async fn connect_with<T, U>(message: U, cfg: IoConfig) -> Result<Io, ConnectError>
where
    T: Address,
    Connect<T>: From<U>,
{
    Connector::with(cfg).connect(message).await
}
