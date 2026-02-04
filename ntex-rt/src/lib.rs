//! A runtime implementation that runs everything on the current thread.
#![deny(clippy::pedantic)]
#![allow(
    clippy::missing_fields_in_debug,
    clippy::must_use_candidate,
    clippy::missing_errors_doc
)]

mod arbiter;
mod builder;
mod driver;
mod handle;
mod pool;
mod system;
mod task;

mod rt;
pub mod rt_default;

#[cfg(feature = "compio")]
pub mod rt_compio;
#[cfg(feature = "tokio")]
pub mod rt_tokio;

pub use self::arbiter::Arbiter;
pub use self::builder::{Builder, SystemRunner};
pub use self::driver::{BlockFuture, Driver, DriverType, Notify, PollResult, Runner};
pub use self::pool::{BlockingError, BlockingResult};
pub use self::rt::{Runtime, RuntimeBuilder};
pub use self::system::{Id, PingRecord, System};
pub use self::task::{task_callbacks, task_opt_callbacks};

/// Spawns a blocking task in a new thread, and wait for it.
///
/// The task will not be cancelled even if the future is dropped.
pub fn spawn_blocking<F, R>(f: F) -> BlockingResult<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    System::current().spawn_blocking(f)
}

#[cfg(feature = "tokio")]
pub use self::rt_tokio::*;

#[cfg(all(feature = "compio", not(feature = "tokio")))]
pub use self::rt_compio::*;

#[cfg(all(not(feature = "tokio"), not(feature = "compio")))]
pub use self::rt_default::*;
