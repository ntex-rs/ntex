//! Utilities for ntex framework
#![deny(rust_2018_idioms, unreachable_pub, missing_debug_implementations)]

pub mod channel;
pub mod future;
pub mod services;
pub mod task;
pub mod time;
pub mod worker;

pub use futures_core::{ready, Stream};
pub use futures_sink::Sink;
pub use ntex_rt::spawn;

pub type HashMap<K, V> = std::collections::HashMap<K, V, fxhash::FxBuildHasher>;
pub type HashSet<V> = std::collections::HashSet<V, fxhash::FxBuildHasher>;
