//! Communication primitives

mod cell;
pub mod condition;
pub mod oneshot;
pub mod pool;

/// Error returned from a [`Receiver`](Receiver) when the corresponding
/// [`Sender`](Sender) is dropped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Canceled;

impl std::fmt::Display for Canceled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::write!(f, "oneshot canceled")
    }
}

impl std::error::Error for Canceled {}
