//! Tcp connector service
mod error;
mod message;
mod resolve;
mod service;
mod uri;

pub use self::error::{ConnectError, ConnectServiceError};
pub use self::message::{Address, Connect};
pub use self::service::{Connector, ConnectorService};

use ntex_error::Error;
use ntex_io::Io;
use ntex_service::cfg::SharedCfg;

/// Resolve and connect to remote host
pub async fn connect<T, U>(message: U) -> Result<Io, Error<ConnectError>>
where
    T: Address,
    Connect<T>: From<U>,
{
    ConnectorService::new().connect(message).await
}

/// Resolve and connect to remote host
pub async fn connect_with<T, U>(
    message: U,
    cfg: SharedCfg,
) -> Result<Io, Error<ConnectError>>
where
    T: Address,
    Connect<T>: From<U>,
{
    ConnectorService::with(cfg).connect(message).await
}
