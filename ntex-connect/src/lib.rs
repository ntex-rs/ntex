//! Tcp connector service
#[macro_use]
extern crate log;

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
    service::ConnectServiceResponse::new(Box::pin(Resolver::new().lookup(message.into())))
        .await
}

#[allow(unused_imports)]
#[doc(hidden)]
pub mod net {
    use super::*;

    #[cfg(feature = "tokio")]
    pub use ntex_tokio::*;

    #[cfg(all(
        feature = "async-std",
        not(feature = "tokio"),
        not(feature = "glommio")
    ))]
    pub use ntex_async_std::*;

    #[cfg(all(
        feature = "glommio",
        not(feature = "tokio"),
        not(feature = "async-std")
    ))]
    pub use ntex_glommio::*;

    #[cfg(all(
        not(feature = "tokio"),
        not(feature = "async-std"),
        not(feature = "glommio")
    ))]
    /// Opens a TCP connection to a remote host.
    pub async fn tcp_connect(_: std::net::SocketAddr) -> std::io::Result<Io> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "runtime is not configure",
        ))
    }

    #[cfg(all(
        not(feature = "tokio"),
        not(feature = "async-std"),
        not(feature = "glommio")
    ))]
    /// Opens a TCP connection to a remote host and use specified memory pool.
    pub async fn tcp_connect_in(
        _: std::net::SocketAddr,
        _: ntex_bytes::PoolRef,
    ) -> std::io::Result<Io> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "runtime is not configure",
        ))
    }

    #[cfg(unix)]
    #[cfg(all(
        not(feature = "tokio"),
        not(feature = "async-std"),
        not(feature = "glommio")
    ))]
    /// Opens a unix stream connection.
    pub async fn unix_connect<'a, P>(_: P) -> std::io::Result<Io>
    where
        P: AsRef<std::path::Path> + 'a,
    {
        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "runtime is not configure",
        ))
    }

    #[cfg(unix)]
    #[cfg(all(
        not(feature = "tokio"),
        not(feature = "async-std"),
        not(feature = "glommio")
    ))]
    /// Opens a unix stream connection and specified memory pool.
    pub async fn unix_connect_in<'a, P>(_: P, _: ntex_bytes::PoolRef) -> std::io::Result<Io>
    where
        P: AsRef<std::path::Path> + 'a,
    {
        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "runtime is not configure",
        ))
    }

    #[cfg(all(
        not(feature = "tokio"),
        not(feature = "async-std"),
        not(feature = "glommio")
    ))]
    /// Convert std TcpStream to tokio's TcpStream
    pub fn from_tcp_stream(_: std::net::TcpStream) -> std::io::Result<Io> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "runtime is not configure",
        ))
    }

    #[cfg(unix)]
    #[cfg(all(
        not(feature = "tokio"),
        not(feature = "async-std"),
        not(feature = "glommio")
    ))]
    /// Convert std UnixStream to tokio's UnixStream
    pub fn from_unix_stream(_: std::os::unix::net::UnixStream) -> std::io::Result<Io> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "runtime is not configure",
        ))
    }
}
