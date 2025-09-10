//! Utilities for ntex framework
#![deny(rust_2018_idioms, unreachable_pub, missing_debug_implementations)]

pub mod channel;
pub mod error;
pub mod future;
pub mod services;
pub mod task;
pub mod time;

pub use futures_core::Stream;
pub use ntex_rt::spawn;

pub type HashMap<K, V> = std::collections::HashMap<K, V, ahash::RandomState>;
pub type HashSet<V> = std::collections::HashSet<V, ahash::RandomState>;

#[doc(hidden)]
#[deprecated]
pub use std::task::ready;
