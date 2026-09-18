//! Reusable services and middleware.
//!
//! This module includes request buffering, in-flight request limits,
//! keep-alive handling, retries, per-request extensions, and timeouts.

pub mod buffer;
mod extensions;
pub mod inflight;
pub mod keepalive;
pub mod onerequest;
pub mod retry;
pub mod timeout;

#[doc(hidden)]
pub mod counter;

pub use self::counter::{Counter, CounterGuard};
pub use self::extensions::Extensions;
