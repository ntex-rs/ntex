//! Tcp connector service
use std::future::Future;

mod error;
mod message;
mod resolve;
mod service;
mod uri;

#[cfg(feature = "openssl")]
pub mod openssl;

#[cfg(feature = "rustls")]
pub mod rustls;

use crate::rt::net::TcpStream;

pub use self::error::ConnectError;
pub use self::message::{Address, Connect};
pub use self::resolve::Resolver;
pub use self::service::Connector;

/// Resolve and connect to remote host
pub fn connect<T, U>(message: U) -> impl Future<Output = Result<TcpStream, ConnectError>>
where
    T: Address + 'static,
    Connect<T>: From<U>,
{
    service::ConnectServiceResponse::new(Box::pin(
        Resolver::new().lookup(message.into()),
    ))
}
