use std::future::Future;
use std::task::{Context, Poll};
use std::{cell::RefCell, io, mem, net, net::SocketAddr, path::Path, pin::Pin, rc::Rc};

use async_oneshot as oneshot;
use ntex_bytes::PoolRef;
use ntex_io::Io;
use ntex_util::future::lazy;
pub use tok_io::task::{spawn_blocking, JoinError, JoinHandle};
use tok_io::{runtime, task::LocalSet};

use crate::{Runtime, Signal};

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

thread_local! {
    static SRUN: RefCell<bool> = RefCell::new(false);
    static SHANDLERS: Rc<RefCell<Vec<oneshot::Sender<Signal>>>> = Default::default();
}

/// Register signal handler.
///
/// Signals are handled by oneshots, you have to re-register
/// after each signal.
pub fn signal() -> Option<oneshot::Receiver<Signal>> {
    if !SRUN.with(|v| *v.borrow()) {
        spawn(Signals::new());
    }
    SHANDLERS.with(|handlers| {
        let (tx, rx) = oneshot::oneshot();
        handlers.borrow_mut().push(tx);
        Some(rx)
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

struct Signals {
    #[cfg(not(unix))]
    signal: Pin<Box<dyn Future<Output = io::Result<()>>>>,
    #[cfg(unix)]
    signals: Vec<(Signal, tok_io::signal::unix::Signal)>,
}

impl Signals {
    pub(super) fn new() -> Signals {
        SRUN.with(|h| *h.borrow_mut() = true);

        #[cfg(not(unix))]
        {
            Signals {
                signal: Box::pin(tok_io::signal::ctrl_c()),
            }
        }

        #[cfg(unix)]
        {
            use tok_io::signal::unix;

            let sig_map = [
                (unix::SignalKind::interrupt(), Signal::Int),
                (unix::SignalKind::hangup(), Signal::Hup),
                (unix::SignalKind::terminate(), Signal::Term),
                (unix::SignalKind::quit(), Signal::Quit),
            ];

            let mut signals = Vec::new();
            for (kind, sig) in sig_map.iter() {
                match unix::signal(*kind) {
                    Ok(stream) => signals.push((*sig, stream)),
                    Err(e) => log::error!(
                        "Cannot initialize stream handler for {:?} err: {}",
                        sig,
                        e
                    ),
                }
            }

            Signals { signals }
        }
    }
}

impl Drop for Signals {
    fn drop(&mut self) {
        SRUN.with(|h| *h.borrow_mut() = false);
    }
}

impl Future for Signals {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        #[cfg(not(unix))]
        {
            if self.signal.as_mut().poll(cx).is_ready() {
                let handlers = SHANDLERS.with(|h| mem::take(&mut *h.borrow_mut()));
                for mut sender in handlers {
                    let _ = sender.send(Signal::Int);
                }
            }
            Poll::Pending
        }
        #[cfg(unix)]
        {
            for (sig, fut) in self.signals.iter_mut() {
                if Pin::new(fut).poll_recv(cx).is_ready() {
                    let handlers = SHANDLERS.with(|h| mem::take(&mut *h.borrow_mut()));
                    for mut sender in handlers {
                        let _ = sender.send(*sig);
                    }
                }
            }
            Poll::Pending
        }
    }
}
