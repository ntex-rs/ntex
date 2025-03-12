//! Communication primitives

mod cell;
pub mod condition;
pub mod inplace;
pub mod mpsc;
pub mod oneshot;
pub mod pool;

/// Error returned from a `Receiver` when the corresponding
/// `Sender` is dropped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Canceled;

impl std::fmt::Display for Canceled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::write!(f, "oneshot canceled")
    }
}

impl std::error::Error for Canceled {}
