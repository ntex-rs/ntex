use std::{io::Result, net, net::SocketAddr};

use ntex_bytes::PoolRef;
use ntex_io::Io;

mod io;

/// Tcp stream wrapper for compio TcpStream
struct TcpStream(compio_net::TcpStream);

#[cfg(unix)]
/// Tcp stream wrapper for compio UnixStream
struct UnixStream(compio_net::UnixStream);

/// Opens a TCP connection to a remote host.
pub async fn tcp_connect(addr: SocketAddr) -> Result<Io> {
    let sock = compio_net::TcpStream::connect(addr).await?;
    Ok(Io::new(TcpStream(sock)))
}

/// Opens a TCP connection to a remote host and use specified memory pool.
pub async fn tcp_connect_in(addr: SocketAddr, pool: PoolRef) -> Result<Io> {
    let sock = compio_net::TcpStream::connect(addr).await?;
    Ok(Io::with_memory_pool(TcpStream(sock), pool))
}

#[cfg(unix)]
/// Opens a unix stream connection.
pub async fn unix_connect<'a, P>(addr: P) -> Result<Io>
where
    P: AsRef<std::path::Path> + 'a,
{
    let sock = compio_net::UnixStream::connect(addr).await?;
    Ok(Io::new(UnixStream(sock)))
}

#[cfg(unix)]
/// Opens a unix stream connection and specified memory pool.
pub async fn unix_connect_in<'a, P>(addr: P, pool: PoolRef) -> Result<Io>
where
    P: AsRef<std::path::Path> + 'a,
{
    let sock = compio_net::UnixStream::connect(addr).await?;
    Ok(Io::with_memory_pool(UnixStream(sock), pool))
}

/// Convert std TcpStream to tokio's TcpStream
pub fn from_tcp_stream(stream: net::TcpStream) -> Result<Io> {
    stream.set_nodelay(true)?;
    Ok(Io::new(TcpStream(compio_net::TcpStream::from_std(stream)?)))
}

#[cfg(unix)]
/// Convert std UnixStream to tokio's UnixStream
pub fn from_unix_stream(stream: std::os::unix::net::UnixStream) -> Result<Io> {
    Ok(Io::new(UnixStream(compio_net::UnixStream::from_std(
        stream,
    )?)))
}
