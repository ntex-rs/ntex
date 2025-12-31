use std::{io::Result, net, net::SocketAddr};

use ntex_io::Io;
use ntex_service::cfg::SharedCfg;

mod io;

pub use self::io::{SocketOptions, TokioIoBoxed};

pub struct TcpStream(tokio::net::TcpStream);

#[cfg(unix)]
pub struct UnixStream(tokio::net::UnixStream);

/// Runs the provided future, blocking the current thread until the future
/// completes.
pub fn block_on<F: Future<Output = ()>>(fut: F) {
    if let Ok(hnd) = tokio::runtime::Handle::try_current() {
        log::debug!("Use existing tokio runtime and block on future");
        hnd.block_on(tokio::task::LocalSet::new().run_until(fut));
    } else {
        log::debug!("Create tokio runtime and block on future");

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio::task::LocalSet::new().block_on(&rt, fut);
    }
}

/// Opens a TCP connection to a remote host.
pub async fn tcp_connect(addr: SocketAddr, cfg: SharedCfg) -> Result<Io> {
    let sock = tokio::net::TcpStream::connect(addr).await?;
    sock.set_nodelay(true)?;
    Ok(Io::new(TcpStream(sock), cfg))
}

#[cfg(unix)]
/// Opens a unix stream connection.
pub async fn unix_connect<'a, P>(addr: P, cfg: SharedCfg) -> Result<Io>
where
    P: AsRef<std::path::Path> + 'a,
{
    let sock = tokio::net::UnixStream::connect(addr).await?;
    Ok(Io::new(UnixStream(sock), cfg))
}

/// Convert std TcpStream to tokio's TcpStream
pub fn from_tcp_stream(stream: net::TcpStream, cfg: SharedCfg) -> Result<Io> {
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
    cfg: SharedCfg,
) -> Result<Io> {
    stream.set_nonblocking(true)?;
    Ok(Io::new(
        UnixStream(tokio::net::UnixStream::from_std(stream)?),
        cfg,
    ))
}

impl From<tokio::net::TcpStream> for TcpStream {
    fn from(stream: tokio::net::TcpStream) -> Self {
        Self(stream)
    }
}

#[cfg(unix)]
impl From<tokio::net::UnixStream> for UnixStream {
    fn from(stream: tokio::net::UnixStream) -> Self {
        Self(stream)
    }
}
