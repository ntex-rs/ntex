//! Asynchronous communication primitives.
//!
//! These channels are primarily intended for local, single-threaded ntex
//! tasks. Individual modules document their ownership and backpressure
//! behavior.

mod cell;

pub mod bstream;
pub mod condition;
pub mod inplace;
pub mod mpsc;
pub mod oneshot;
pub mod pool;

/// Error returned when a channel is canceled before producing a value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Canceled;

impl std::fmt::Display for Canceled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::write!(f, "oneshot canceled")
    }
}

impl std::error::Error for Canceled {}
