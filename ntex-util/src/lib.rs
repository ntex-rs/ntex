//! Utilities for ntex framework
#![allow(clippy::return_self_not_must_use)]
pub mod channel;
pub mod future;
pub mod services;
pub mod task;
pub mod time;

pub use futures_core::{ready, Stream};
pub use futures_sink::Sink;
pub use ntex_rt::spawn;

pub type HashMap<K, V> = std::collections::HashMap<K, V, fxhash::FxBuildHasher>;
pub type HashSet<V> = std::collections::HashSet<V, fxhash::FxBuildHasher>;
