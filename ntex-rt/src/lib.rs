//! A runtime implementation that runs everything on the current thread.
use ntex_util::future::lazy;
use std::future::Future;

mod arbiter;
mod builder;
mod runtime;
mod system;

pub use self::arbiter::Arbiter;
pub use self::builder::{Builder, SystemRunner};
pub use self::runtime::Runtime;
pub use self::system::System;

/// Spawn a future on the current thread. This does not create a new Arbiter
/// or Arbiter address, it is simply a helper for spawning futures on the current
/// thread.
///
/// # Panics
///
/// This function panics if ntex system is not running.
#[inline]
pub fn spawn<F>(f: F) -> self::task::JoinHandle<F::Output>
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
    tokio::task::spawn_local(async move {
        let r = lazy(|_| f()).await;
        r.await
    })
}

/// Asynchronous signal handling
pub mod signal {
    #[cfg(unix)]
    pub mod unix {
        pub use tokio::signal::unix::*;
    }
    pub use tokio::signal::ctrl_c;
}

/// TCP/UDP/Unix bindings
pub mod net {
    pub use tokio::net::UdpSocket;
    pub use tokio::net::{TcpListener, TcpStream};

    #[cfg(unix)]
    pub mod unix {
        pub use tokio::net::{UnixDatagram, UnixListener, UnixStream};
    }

    #[cfg(unix)]
    pub use self::unix::*;
}

/// Utilities for tracking time.
pub mod time {
    pub use tokio::time::Instant;
    pub use tokio::time::{interval, interval_at, Interval};
    pub use tokio::time::{sleep, sleep_until, Sleep};
    #[doc(hidden)]
    pub use tokio::time::{
        sleep as delay_for, sleep_until as delay_until, Sleep as Delay,
    };
    pub use tokio::time::{timeout, Timeout};
}

/// Task management.
pub mod task {
    pub use tokio::task::{spawn_blocking, yield_now, JoinError, JoinHandle};
}
