use std::{io::Result, net, net::SocketAddr};

use ntex_io::{Io, IoConfig};

mod io;

pub use self::io::{SocketOptions, TokioIoBoxed};

struct TcpStream(tokio::net::TcpStream);

#[cfg(unix)]
struct UnixStream(tokio::net::UnixStream);

/// Opens a TCP connection to a remote host.
pub async fn tcp_connect(addr: SocketAddr, cfg: IoConfig) -> Result<Io> {
    let sock = tokio::net::TcpStream::connect(addr).await?;
    sock.set_nodelay(true)?;
    Ok(Io::new(TcpStream(sock), cfg))
}

#[cfg(unix)]
/// Opens a unix stream connection.
pub async fn unix_connect<'a, P>(addr: P, cfg: IoConfig) -> Result<Io>
where
    P: AsRef<std::path::Path> + 'a,
{
    let sock = tokio::net::UnixStream::connect(addr).await?;
    Ok(Io::new(UnixStream(sock), cfg))
}

/// Convert std TcpStream to tokio's TcpStream
pub fn from_tcp_stream(stream: net::TcpStream, cfg: IoConfig) -> Result<Io> {
    stream.set_nonblocking(true)?;
    stream.set_nodelay(true)?;
    Ok(Io::new(
        TcpStream(tokio::net::TcpStream::from_std(stream)?),
        cfg,
    ))
}

#[cfg(unix)]
/// Convert std UnixStream to tokio's UnixStream
pub fn from_unix_stream(
    stream: std::os::unix::net::UnixStream,
    cfg: IoConfig,
) -> Result<Io> {
    stream.set_nonblocking(true)?;
    Ok(Io::new(
        UnixStream(tokio::net::UnixStream::from_std(stream)?),
        cfg,
    ))
}
