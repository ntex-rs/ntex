//! Utilities for ntex framework
#![deny(rust_2018_idioms, unreachable_pub, missing_debug_implementations)]

#[doc(hidden)]
#[deprecated]
pub use std::task::ready;

pub mod channel;
pub mod future;
pub mod services;
pub mod task;
pub mod time;

pub use futures_core::Stream;
pub use futures_sink::Sink;
pub use ntex_rt::spawn;

pub type HashMap<K, V> = std::collections::HashMap<K, V, fxhash::FxBuildHasher>;
pub type HashSet<V> = std::collections::HashSet<V, fxhash::FxBuildHasher>;
