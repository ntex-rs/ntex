use std::{future::Future, io, net, net::SocketAddr, path::Path, pin::Pin};

use ntex_bytes::PoolRef;
use ntex_io::Io;
use ntex_util::future::lazy;
use tok_io::{runtime, task::LocalSet};

use crate::Runtime;

/// Create new single-threaded tokio runtime.
pub fn create_runtime() -> Box<dyn Runtime> {
    Box::new(TokioRuntime::new().unwrap())
}

/// Opens a TCP connection to a remote host.
pub fn tcp_connect(
    addr: SocketAddr,
) -> Pin<Box<dyn Future<Output = Result<Io, io::Error>>>> {
    Box::pin(async move {
        let sock = tok_io::net::TcpStream::connect(addr).await?;
        sock.set_nodelay(true)?;
        Ok(Io::new(sock))
    })
}

/// Opens a TCP connection to a remote host and use specified memory pool.
pub fn tcp_connect_in(
    addr: SocketAddr,
    pool: PoolRef,
) -> Pin<Box<dyn Future<Output = Result<Io, io::Error>>>> {
    Box::pin(async move {
        let sock = tok_io::net::TcpStream::connect(addr).await?;
        sock.set_nodelay(true)?;
        Ok(Io::with_memory_pool(sock, pool))
    })
}

#[cfg(unix)]
/// Opens a unix stream connection.
pub fn unix_connect<P>(addr: P) -> Pin<Box<dyn Future<Output = Result<Io, io::Error>>>>
where
    P: AsRef<Path> + 'static,
{
    Box::pin(async move {
        let sock = tok_io::net::UnixStream::connect(addr).await?;
        Ok(Io::new(sock))
    })
}

/// Convert std TcpStream to tokio's TcpStream
pub fn from_tcp_stream(stream: net::TcpStream) -> Result<Io, io::Error> {
    stream.set_nonblocking(true)?;
    stream.set_nodelay(true)?;
    Ok(Io::new(tok_io::net::TcpStream::from_std(stream)?))
}

#[cfg(unix)]
/// Convert std UnixStream to tokio's UnixStream
pub fn from_unix_stream(
    stream: std::os::unix::net::UnixStream,
) -> Result<Io, io::Error> {
    stream.set_nonblocking(true)?;
    Ok(Io::new(tok_io::net::UnixStream::from_std(stream)?))
}

/// Spawn a future on the current thread. This does not create a new Arbiter
/// or Arbiter address, it is simply a helper for spawning futures on the current
/// thread.
///
/// # Panics
///
/// This function panics if ntex system is not running.
#[inline]
pub fn spawn<F>(f: F) -> tok_io::task::JoinHandle<F::Output>
where
    F: Future + 'static,
{
    tok_io::task::spawn_local(f)
}

/// Executes a future on the current thread. This does not create a new Arbiter
/// or Arbiter address, it is simply a helper for executing futures on the current
/// thread.
///
/// # Panics
///
/// This function panics if ntex system is not running.
#[inline]
pub fn spawn_fn<F, R>(f: F) -> tok_io::task::JoinHandle<R::Output>
where
    F: FnOnce() -> R + 'static,
    R: Future + 'static,
{
    spawn(async move {
        let r = lazy(|_| f()).await;
        r.await
    })
}

/// Single-threaded tokio runtime.
#[derive(Debug)]
struct TokioRuntime {
    local: LocalSet,
    rt: runtime::Runtime,
}
impl TokioRuntime {
    /// Returns a new runtime initialized with default configuration values.
    fn new() -> io::Result<Self> {
        let rt = runtime::Builder::new_current_thread().enable_io().build()?;

        Ok(Self {
            rt,
            local: LocalSet::new(),
        })
    }
}

impl Runtime for TokioRuntime {
    /// Spawn a future onto the single-threaded runtime.
    fn spawn(&self, future: Pin<Box<dyn Future<Output = ()>>>) {
        self.local.spawn_local(future);
    }

    /// Runs the provided future, blocking the current thread until the future
    /// completes.
    fn block_on(&self, f: Pin<Box<dyn Future<Output = ()>>>) {
        // set ntex-util spawn fn
        ntex_util::set_spawn_fn(|fut| {
            tok_io::task::spawn_local(fut);
        });

        self.local.block_on(&self.rt, f);
    }
}
