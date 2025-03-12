//! Utility for async runtime abstraction

#[cfg(feature = "tokio")]
pub use ntex_tokio::{from_tcp_stream, tcp_connect, tcp_connect_in};

#[cfg(all(unix, feature = "tokio"))]
pub use ntex_tokio::{from_unix_stream, unix_connect, unix_connect_in};

#[cfg(all(feature = "compio", not(feature = "tokio"), not(feature = "neon")))]
pub use ntex_compio::{from_tcp_stream, tcp_connect, tcp_connect_in};

#[cfg(all(
    unix,
    feature = "compio",
    not(feature = "tokio"),
    not(feature = "neon")
))]
pub use ntex_compio::{from_unix_stream, unix_connect, unix_connect_in};

#[cfg(all(not(feature = "tokio"), not(feature = "compio"), not(feature = "neon")))]
mod no_rt {
    use ntex_io::Io;

    /// Opens a TCP connection to a remote host.
    pub async fn tcp_connect(_: std::net::SocketAddr) -> std::io::Result<Io> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "runtime is not configure",
        ))
    }

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

    /// Convert std TcpStream to tokio's TcpStream
    pub fn from_tcp_stream(_: std::net::TcpStream) -> std::io::Result<Io> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "runtime is not configure",
        ))
    }

    #[cfg(unix)]
    /// Convert std UnixStream to tokio's UnixStream
    pub fn from_unix_stream(_: std::os::unix::net::UnixStream) -> std::io::Result<Io> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "runtime is not configure",
        ))
    }
}

#[cfg(all(not(feature = "tokio"), not(feature = "compio"), not(feature = "neon")))]
pub use no_rt::*;
