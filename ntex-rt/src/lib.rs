//! Single-threaded asynchronous runtime infrastructure for ntex.
//!
//! A [`System`] owns the runtime configuration and coordinates one or more
//! [`Arbiter`] execution threads. Each arbiter runs local futures on its own
//! thread, allowing tasks to use types that are not `Send`.
//!
//! Use [`System::build()`] to configure a system, [`spawn()`] to start a local
//! task, and [`spawn_blocking()`] for blocking work.
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
mod driver;
mod handle;
mod pool;
pub mod signals;
mod system;
mod task;

mod rt;
pub mod rt_default;

#[cfg(feature = "compio")]
pub mod rt_compio;
#[cfg(feature = "tokio")]
pub mod rt_tokio;

pub use self::arbiter::{Arbiter, get_item, remove_all_items, set_item, with_item};
pub use self::builder::{Builder, SystemRunner};
pub use self::driver::{BlockFuture, Driver, DriverType, Notify, PollResult, Runner};
pub use self::pool::{BlockingError, BlockingResult, ThreadPool, spawn_blocking};
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
pub(crate) type HashSet<V> = std::collections::HashSet<V, foldhash::fast::RandomState>;
