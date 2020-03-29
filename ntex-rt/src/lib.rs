//! A runtime implementation that runs everything on the current thread.
mod arbiter;
mod builder;
mod runtime;
mod system;

pub use self::arbiter::Arbiter;
pub use self::builder::{Builder, SystemRunner};
pub use self::runtime::Runtime;
pub use self::system::System;

#[cfg(not(test))] // Work around for rust-lang/rust#62127
pub use ntex_macros::{rt_main as main, rt_test as test};

#[doc(hidden)]
pub use actix_threadpool as blocking;

/// Spawns a future on the current arbiter.
///
/// # Panics
///
/// This function panics if actix system is not running.
#[inline]
pub fn spawn<F>(f: F)
where
    F: futures::Future<Output = ()> + 'static,
{
    if !System::is_set() {
        panic!("System is not running");
    }

    Arbiter::spawn(f);
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
    pub use tokio::time::{delay_for, delay_until, Delay};
    pub use tokio::time::{interval, interval_at, Interval};
    pub use tokio::time::{timeout, Timeout};
}
