//! Tcp connector service
use ntex_error::Error;

mod error;
mod message;
mod resolve;
mod service;
mod uri;

pub use self::error::ConnectError;
pub use self::message::{Address, Connect};
pub use self::service::Connector;

use ntex_io::Io;
use ntex_service::cfg::SharedCfg;

/// Resolve and connect to remote host
pub async fn connect<A, U>(message: U) -> Result<Io, Error<ConnectError>>
where
    A: Address,
    Connect<A>: From<U>,
{
    Connector::<A, ()>::new().connect(message).await
}

/// Resolve and connect to remote host
pub async fn connect_with<A, U>(message: U, cfg: SharedCfg) -> Result<Io, Error<ConnectError>>
where
    A: Address,
    Connect<A>: From<U>,
{
    Connector::<A, ()>::with(cfg).connect(message).await
}
