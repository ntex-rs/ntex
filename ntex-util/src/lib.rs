//! Utilities for ntex framework
#![deny(clippy::pedantic)]
#![allow(
    clippy::missing_fields_in_debug,
    clippy::must_use_candidate,
    clippy::return_self_not_must_use,
    clippy::missing_errors_doc
)]

pub mod channel;
pub mod error;
pub mod future;
pub mod services;
pub mod task;
pub mod time;

pub use futures_core::Stream;
pub use ntex_rt::spawn;

pub type HashMap<K, V> = std::collections::HashMap<K, V, foldhash::fast::RandomState>;
pub type HashSet<V> = std::collections::HashSet<V, foldhash::fast::RandomState>;
pub type HashRandomState = foldhash::fast::RandomState;
