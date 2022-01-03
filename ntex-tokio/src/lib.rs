use std::{future::Future, io, net, net::SocketAddr, path::Path};

use ntex_bytes::PoolRef;
use ntex_io::Io;
use ntex_util::future::lazy;
pub use tokio::task::{spawn_blocking, JoinError, JoinHandle};

mod signals;

pub use signals::{signal, Signal};

/// Opens a TCP connection to a remote host.
pub async fn tcp_connect(addr: SocketAddr) -> Result<Io, io::Error> {
    let sock = tokio::net::TcpStream::connect(addr).await?;
    sock.set_nodelay(true)?;
    Ok(Io::new(sock))
}

/// Opens a TCP connection to a remote host and use specified memory pool.
pub async fn tcp_connect_in(addr: SocketAddr, pool: PoolRef) -> Result<Io, io::Error> {
    let sock = tokio::net::TcpStream::connect(addr).await?;
    sock.set_nodelay(true)?;
    Ok(Io::with_memory_pool(sock, pool))
}

#[cfg(unix)]
/// Opens a unix stream connection.
pub async fn unix_connect<'a, P>(addr: P) -> Result<Io, io::Error>
where
    P: AsRef<Path> + 'a,
{
    let sock = tokio::net::UnixStream::connect(addr).await?;
    Ok(Io::new(sock))
}

#[cfg(unix)]
/// Opens a unix stream connection and specified memory pool.
pub async fn unix_connect_in<'a, P>(addr: P, pool: PoolRef) -> Result<Io, io::Error>
where
    P: AsRef<Path> + 'a,
{
    let sock = tokio::net::UnixStream::connect(addr).await?;
    Ok(Io::with_memory_pool(sock, pool))
}

/// Convert std TcpStream to tokio's TcpStream
pub fn from_tcp_stream(stream: net::TcpStream) -> Result<Io, io::Error> {
    stream.set_nonblocking(true)?;
    stream.set_nodelay(true)?;
    Ok(Io::new(tokio::net::TcpStream::from_std(stream)?))
}

#[cfg(unix)]
/// Convert std UnixStream to tokio's UnixStream
pub fn from_unix_stream(stream: std::os::unix::net::UnixStream) -> Result<Io, io::Error> {
    stream.set_nonblocking(true)?;
    Ok(Io::new(tokio::net::UnixStream::from_std(stream)?))
}

/// Spawn a future on the current thread. This does not create a new Arbiter
/// or Arbiter address, it is simply a helper for spawning futures on the current
/// thread.
///
/// # Panics
///
/// This function panics if ntex system is not running.
#[inline]
pub fn spawn<F>(f: F) -> tokio::task::JoinHandle<F::Output>
where
    F: Future + 'static,
{
    tokio::task::spawn_local(f)
}

/// Executes a future on the current thread. This does not create a new Arbiter
/// or Arbiter address, it is simply a helper for executing futures on the current
/// thread.
///
/// # Panics
///
/// This function panics if ntex system is not running.
#[inline]
pub fn spawn_fn<F, R>(f: F) -> tokio::task::JoinHandle<R::Output>
where
    F: FnOnce() -> R + 'static,
    R: Future + 'static,
{
    spawn(async move {
        let r = lazy(|_| f()).await;
        r.await
    })
}
