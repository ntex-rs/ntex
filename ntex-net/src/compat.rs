//! Utility for async runtime abstraction

#[cfg(feature = "tokio")]
pub use ntex_tokio::{from_tcp_stream, tcp_connect};

#[cfg(all(unix, feature = "tokio"))]
pub use ntex_tokio::{from_unix_stream, unix_connect};

#[cfg(all(feature = "compio", not(feature = "tokio"), not(feature = "neon")))]
pub use ntex_compio::{from_tcp_stream, tcp_connect};

#[cfg(all(
    unix,
    feature = "compio",
    not(feature = "tokio"),
    not(feature = "neon")
))]
pub use ntex_compio::{from_unix_stream, unix_connect};

#[cfg(all(not(feature = "tokio"), not(feature = "compio"), not(feature = "neon")))]
mod no_rt {
    use ntex_io::{Io, IoConfig};

    /// Opens a TCP connection to a remote host.
    pub async fn tcp_connect(_: std::net::SocketAddr, _: IoConfig) -> std::io::Result<Io> {
        Err(std::io::Error::other("runtime is not configure"))
    }

    #[cfg(unix)]
    /// Opens a unix stream connection.
    pub async fn unix_connect<'a, P>(_: P, _: IoConfig) -> std::io::Result<Io>
    where
        P: AsRef<std::path::Path> + 'a,
    {
        Err(std::io::Error::other("runtime is not configure"))
    }

    /// Convert std TcpStream to tokio's TcpStream
    pub fn from_tcp_stream(_: std::net::TcpStream, _: IoConfig) -> std::io::Result<Io> {
        Err(std::io::Error::other("runtime is not configure"))
    }

    #[cfg(unix)]
    /// Convert std UnixStream to tokio's UnixStream
    pub fn from_unix_stream(
        _: std::os::unix::net::UnixStream,
        _: IoConfig,
    ) -> std::io::Result<Io> {
        Err(std::io::Error::other("runtime is not configure"))
    }
}

#[cfg(all(not(feature = "tokio"), not(feature = "compio"), not(feature = "neon")))]
pub use no_rt::*;
