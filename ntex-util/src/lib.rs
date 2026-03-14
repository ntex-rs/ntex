//! Utilities for ntex framework
#![deny(clippy::pedantic)]
#![allow(
    clippy::missing_fields_in_debug,
    clippy::must_use_candidate,
    clippy::missing_errors_doc
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

#[deprecated(since = "3.5.0", note = "Use ntex-error 1.2")]
pub mod error {
    pub use ntex_error::{ErrorMessage, ErrorMessageChained, fmt_err, fmt_err_string};
}
