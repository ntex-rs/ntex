//! Utilities for ntex framework
#![deny(clippy::pedantic)]
#![allow(
    async_fn_in_trait,
    clippy::missing_fields_in_debug,
    clippy::must_use_candidate,
    clippy::missing_errors_doc,
    clippy::unused_async_trait_impl
)]

pub mod channel;
pub mod future;
pub mod services;
pub mod task;
pub mod time;

pub use futures_core::Stream;
pub use ntex_rt::spawn;

pub type HashMap<K, V> = std::collections::HashMap<K, V, foldhash::fast::RandomState>;
pub type HashSet<V> = std::collections::HashSet<V, foldhash::fast::RandomState>;
pub type HashRandomState = foldhash::fast::RandomState;
