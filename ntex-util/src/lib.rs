//! Utilities shared by the ntex ecosystem.
//!
//! This crate provides:
//!
//! - [`channel`] for local asynchronous communication primitives
//! - [`future`] for future and stream combinators
//! - [`services`] for reusable service middleware
//! - [`task`] for task wake-up and cooperative yielding
//! - [`time`] for timers, intervals, deadlines, and timeouts
//!
//! Most types in this crate are designed for ntex's single-threaded execution
//! model and therefore do not necessarily implement `Send` or `Sync`.
#![deny(clippy::pedantic)]
#![allow(
    async_fn_in_trait,
    clippy::missing_fields_in_debug,
    clippy::must_use_candidate,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::unused_async_trait_impl
)]
use std::{error::Error, io, rc::Rc};

pub mod channel;
pub mod future;
pub mod services;
pub mod task;
pub mod time;

pub use futures_core::Stream;
pub use ntex_rt::spawn;

#[doc(hidden)]
pub use hashbrown::{Equivalent, hash_map, hash_set};

/// A hash map using ntex's fast, randomly seeded hash state.
pub type HashMap<K, V> = hash_map::HashMap<K, V, foldhash::fast::RandomState>;
/// A hash set using ntex's fast, randomly seeded hash state.
pub type HashSet<V> = hash_set::HashSet<V, foldhash::fast::RandomState>;
/// The hash state used by [`HashMap`] and [`HashSet`].
pub type HashRandomState = foldhash::fast::RandomState;

/// Boxes an error as a dynamically dispatched error.
pub fn dyn_err<E: Error + 'static>(e: E) -> Box<dyn Error> {
    let e: Box<dyn Error> = Box::new(e);
    e
}

/// Wraps an error in a reference-counted, dynamically dispatched error.
pub fn dyn_rc_err<T: Error + 'static>(err: T) -> Rc<dyn Error> {
    Rc::new(err)
}

/// Converts a string into a reference-counted error.
pub fn str_rc_err(s: String) -> Rc<dyn Error> {
    #[derive(thiserror::Error, Debug)]
    #[error("{_0}")]
    struct StringError(String);

    Rc::new(StringError(s))
}

/// Clones an I/O error's kind and debug representation.
///
/// `std::io::Error` is not generally cloneable. The returned error preserves
/// the original [`io::ErrorKind`] and uses the original error's debug output as
/// its message.
pub fn clone_io_error(err: &io::Error) -> io::Error {
    io::Error::new(err.kind(), format!("{err:?}"))
}
