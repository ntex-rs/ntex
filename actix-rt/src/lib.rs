//! A runtime implementation that runs everything on the current thread.

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
