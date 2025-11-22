use std::{io::Result, net, net::SocketAddr};

use ntex_io::{Io, IoConfig};

mod io;

/// Tcp stream wrapper for compio TcpStream
struct TcpStream(compio_net::TcpStream);

#[cfg(unix)]
/// Tcp stream wrapper for compio UnixStream
struct UnixStream(compio_net::UnixStream);

/// Opens a TCP connection to a remote host.
pub async fn tcp_connect(addr: SocketAddr, cfg: IoConfig) -> Result<Io> {
    let sock = compio_net::TcpStream::connect(addr).await?;
    Ok(Io::new(TcpStream(sock), cfg))
}

#[cfg(unix)]
/// Opens a unix stream connection.
pub async fn unix_connect<'a, P>(addr: P, cfg: IoConfig) -> Result<Io>
where
    P: AsRef<std::path::Path> + 'a,
{
    let sock = compio_net::UnixStream::connect(addr).await?;
    Ok(Io::new(UnixStream(sock), cfg))
}

/// Convert std TcpStream to tokio's TcpStream
pub fn from_tcp_stream(stream: net::TcpStream, cfg: IoConfig) -> Result<Io> {
    stream.set_nodelay(true)?;
    Ok(Io::new(
        TcpStream(compio_net::TcpStream::from_std(stream)?),
        cfg,
    ))
}

#[cfg(unix)]
/// Convert std UnixStream to tokio's UnixStream
pub fn from_unix_stream(
    stream: std::os::unix::net::UnixStream,
    cfg: IoConfig,
) -> Result<Io> {
    Ok(Io::new(
        UnixStream(compio_net::UnixStream::from_std(stream)?),
        cfg,
    ))
}
