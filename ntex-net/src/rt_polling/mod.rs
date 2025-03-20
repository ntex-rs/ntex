use std::{io::Result, net, net::SocketAddr};

use ntex_bytes::PoolRef;
use ntex_io::Io;
use socket2::Socket;

pub(crate) mod connect;
mod driver;
mod io;

#[cfg(not(target_pointer_width = "64"))]
compile_error!("Only 64bit platforms are supported");

/// Tcp stream wrapper for neon TcpStream
struct TcpStream(socket2::Socket);

/// Tcp stream wrapper for neon UnixStream
struct UnixStream(socket2::Socket);

/// Opens a TCP connection to a remote host.
pub async fn tcp_connect(addr: SocketAddr) -> Result<Io> {
    let sock = crate::helpers::connect(addr).await?;
    Ok(Io::new(TcpStream(crate::helpers::prep_socket(sock)?)))
}

/// Opens a TCP connection to a remote host and use specified memory pool.
pub async fn tcp_connect_in(addr: SocketAddr, pool: PoolRef) -> Result<Io> {
    let sock = crate::helpers::connect(addr).await?;
    Ok(Io::with_memory_pool(
        TcpStream(crate::helpers::prep_socket(sock)?),
        pool,
    ))
}

/// Opens a unix stream connection.
pub async fn unix_connect<'a, P>(addr: P) -> Result<Io>
where
    P: AsRef<std::path::Path> + 'a,
{
    let sock = crate::helpers::connect_unix(addr).await?;
    Ok(Io::new(UnixStream(crate::helpers::prep_socket(sock)?)))
}

/// Opens a unix stream connection and specified memory pool.
pub async fn unix_connect_in<'a, P>(addr: P, pool: PoolRef) -> Result<Io>
where
    P: AsRef<std::path::Path> + 'a,
{
    let sock = crate::helpers::connect_unix(addr).await?;
    Ok(Io::with_memory_pool(
        UnixStream(crate::helpers::prep_socket(sock)?),
        pool,
    ))
}

/// Convert std TcpStream to TcpStream
pub fn from_tcp_stream(stream: net::TcpStream) -> Result<Io> {
    stream.set_nodelay(true)?;
    Ok(Io::new(TcpStream(crate::helpers::prep_socket(
        Socket::from(stream),
    )?)))
}

/// Convert std UnixStream to UnixStream
pub fn from_unix_stream(stream: std::os::unix::net::UnixStream) -> Result<Io> {
    Ok(Io::new(UnixStream(crate::helpers::prep_socket(
        Socket::from(stream),
    )?)))
}
