//! A runtime implementation that runs everything on the current thread.
#![deny(rust_2018_idioms, warnings)]
#![allow(clippy::type_complexity)]

#[cfg(not(test))] // Work around for rust-lang/rust#62127
pub use actix_macros::{main, test};

mod arbiter;
mod builder;
mod runtime;
mod system;

pub use self::arbiter::Arbiter;
pub use self::builder::{Builder, SystemRunner};
pub use self::runtime::Runtime;
pub use self::system::System;

#[doc(hidden)]
pub use actix_threadpool as blocking;

/// Spawns a future on the current arbiter.
///
/// # Panics
///
/// This function panics if actix system is not running.
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
        pub use tokio_net::signal::unix::*;
    }
    pub use tokio_net::signal::{ctrl_c, CtrlC};
}

/// TCP/UDP/Unix bindings
pub mod net {
    pub use tokio::net::UdpSocket;
    pub use tokio::net::{TcpListener, TcpStream};

    #[cfg(unix)]
    mod unix {
        pub use tokio::net::{UnixDatagram, UnixListener, UnixStream};
    }

    pub use self::unix::*;
}

/// Utilities for tracking time.
pub mod time {
    use std::time::{Duration, Instant};

    pub use tokio_timer::Interval;
    pub use tokio_timer::{delay, delay_for, Delay};
    pub use tokio_timer::{timeout, Timeout};

    /// Creates new `Interval` that yields with interval of `duration`. The first
    /// tick completes immediately.
    pub fn interval(duration: Duration) -> Interval {
        Interval::new(Instant::now(), duration)
    }

    /// Creates new `Interval` that yields with interval of `period` with the
    /// first tick completing at `at`.
    pub fn interval_at(start: Instant, duration: Duration) -> Interval {
        Interval::new(start, duration)
    }
}
