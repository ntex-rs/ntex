use std::{io::Result, net, net::SocketAddr};

use ntex_bytes::PoolRef;
use ntex_io::Io;

mod io;
mod signals;

pub use self::signals::{signal, Signal};

#[derive(Clone)]
struct TcpStream(async_std::net::TcpStream);

#[cfg(unix)]
#[derive(Clone)]
struct UnixStream(async_std::os::unix::net::UnixStream);

/// Opens a TCP connection to a remote host.
pub async fn tcp_connect(addr: SocketAddr) -> Result<Io> {
    let sock = async_std::net::TcpStream::connect(addr).await?;
    sock.set_nodelay(true)?;
    Ok(Io::new(TcpStream(sock)))
}

/// Opens a TCP connection to a remote host and use specified memory pool.
pub async fn tcp_connect_in(addr: SocketAddr, pool: PoolRef) -> Result<Io> {
    let sock = async_std::net::TcpStream::connect(addr).await?;
    sock.set_nodelay(true)?;
    Ok(Io::with_memory_pool(TcpStream(sock), pool))
}

#[cfg(unix)]
/// Opens a unix stream connection.
pub async fn unix_connect<P>(addr: P) -> Result<Io>
where
    P: AsRef<async_std::path::Path>,
{
    let sock = async_std::os::unix::net::UnixStream::connect(addr).await?;
    Ok(Io::new(UnixStream(sock)))
}

#[cfg(unix)]
/// Opens a unix stream connection and specified memory pool.
pub async fn unix_connect_in<P>(addr: P, pool: PoolRef) -> Result<Io>
where
    P: AsRef<async_std::path::Path>,
{
    let sock = async_std::os::unix::net::UnixStream::connect(addr).await?;
    Ok(Io::with_memory_pool(UnixStream(sock), pool))
}

/// Convert std TcpStream to async-std's TcpStream
pub fn from_tcp_stream(stream: net::TcpStream) -> Result<Io> {
    stream.set_nonblocking(true)?;
    stream.set_nodelay(true)?;
    Ok(Io::new(TcpStream(async_std::net::TcpStream::from(stream))))
}

#[cfg(unix)]
/// Convert std UnixStream to async-std's UnixStream
pub fn from_unix_stream(stream: std::os::unix::net::UnixStream) -> Result<Io> {
    stream.set_nonblocking(true)?;
    Ok(Io::new(UnixStream(From::from(stream))))
}
