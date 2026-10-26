//! Single-threaded asynchronous runtime infrastructure for ntex.
//!
//! A [`System`] owns the runtime configuration and coordinates one or more
//! [`Arbiter`] execution threads. Each arbiter runs local futures on its own
//! thread, allowing tasks to use types that are not `Send`.
//!
//! Use [`System::build()`] to configure a system, [`spawn()`] to start a local
//! task, and [`spawn_blocking()`] for blocking work.
//!
//! Local [`JoinHandle`]s are neither `Send` nor `Sync`, even for `Send` outputs:
//!
//! ```compile_fail
//! fn send<T: Send>() {}
//! send::<ntex_rt::JoinHandle<usize>>();
//! ```
//!
//! ```compile_fail
//! fn sync<T: Sync>() {}
//! sync::<ntex_rt::JoinHandle<usize>>();
//! ```
//!
//! [`Handle::spawn()`] submits a `Send` future to the handle's runtime and
//! returns a [`RemoteJoinHandle`] for the calling thread. On the runtime's own thread
//! the task is spawned directly. From other threads the future and later its
//! `Send` output share one allocation, which is passed to the runtime thread:
//!
//! ```no_run
//! # async fn example(handle: ntex_rt::Handle) {
//! let task = handle.spawn(async { 42 });
//! assert_eq!(task.await.unwrap(), 42);
//! # }
//! ```
//!
//! Dropping the handle detaches the task, [`RemoteJoinHandle::cancel()`] cancels it.
//! A submitted future is polled and dropped on the runtime thread; shutdown or
//! cancellation before producing a result yields [`JoinError`]. No task is
//! guaranteed to finish. Rejected submissions are never polled and their
//! futures may be dropped on the submitting thread.
//!
//! Use [`Handle::spawn_detached()`] for fire-and-forget work. Its output need
//! not be `Send` and is discarded on the owner.
//!
//! Remote join handles also remain on their calling thread, even when the
//! submission uses the runtime's local fast path:
//!
//! ```compile_fail
//! fn send<T: Send>() {}
//! send::<ntex_rt::RemoteJoinHandle<usize>>();
//! ```
//!
//! ```compile_fail
//! fn sync<T: Sync>() {}
//! sync::<ntex_rt::RemoteJoinHandle<usize>>();
//! ```
//!
//! ```compile_fail
//! # fn example(handle: ntex_rt::Handle) {
//! handle.spawn(async { std::rc::Rc::new(42) });
//! # }
//! ```
//!
//! [`LocalWaker`] owns a thread-confined wake handle. It specializes native
//! task wakes on their owner thread and preserves standard waker behavior
//! for other runtimes and wrapped wakers.
//!
//! Runtime backends are selected with Cargo features:
//!
//! - no runtime feature uses the native ntex runtime
//! - `tokio` uses the Tokio-based runtime adapter
//! - `compio` uses the Compio-based runtime adapter
#![deny(clippy::pedantic)]
#![allow(
    clippy::missing_errors_doc,
    clippy::missing_fields_in_debug,
    clippy::must_use_candidate
)]

mod arbiter;
mod builder;
mod busy;
#[cfg(target_os = "linux")]
mod capture;
mod driver;
mod handle;
mod local_waker;
mod metrics;
mod pool;
mod remote;
pub mod signals;
mod system;
mod task;
#[cfg(test)]
mod testing;
mod watchdog;

mod rt;
pub mod rt_default;

#[cfg(feature = "compio")]
pub mod rt_compio;
#[cfg(feature = "tokio")]
pub mod rt_tokio;

pub use self::arbiter::{Arbiter, get_item, remove_all_items, set_item, with_item};
pub use self::builder::{Builder, SystemRunner};
pub use self::driver::{BlockFuture, Driver, DriverType, Notify, PollResult, Runner};
pub use self::local_waker::LocalWaker;
pub use self::metrics::{HISTOGRAM_BUCKETS, Histogram, LatencyMetrics};
pub use self::pool::{BlockingError, BlockingResult, PoolMetrics, ThreadPool, spawn_blocking};
pub use self::rt::{Runtime, RuntimeBuilder};
pub use self::system::{Id, PingRecord, System};
pub use self::task::{task_callbacks, task_opt_callbacks};

#[cfg(feature = "tokio")]
pub use self::rt_tokio::*;

#[cfg(all(feature = "compio", not(feature = "tokio")))]
pub use self::rt_compio::*;

#[cfg(all(not(feature = "tokio"), not(feature = "compio")))]
pub use self::rt_default::*;

pub(crate) type HashMap<K, V> = std::collections::HashMap<K, V, foldhash::fast::RandomState>;

/// Runs `f` while an earlier panic is pending, the process aborts if `f`
/// panics.
pub(crate) fn abort_on_panic<R>(f: impl FnOnce() -> R) -> R {
    struct Abort;

    impl Drop for Abort {
        fn drop(&mut self) {
            abort_on_second_panic();
        }
    }

    let guard = Abort;
    let result = f();
    std::mem::forget(guard);
    result
}

/// Aborts the process on a panic raised while an earlier panic is unwinding
/// or held to be resumed.
#[cold]
pub(crate) fn abort_on_second_panic() -> ! {
    if let Err(payload) = std::panic::catch_unwind(|| {
        log::error!("Panic during shutdown after an earlier panic; aborting");
    }) {
        std::mem::forget(payload);
    }
    std::process::abort();
}
