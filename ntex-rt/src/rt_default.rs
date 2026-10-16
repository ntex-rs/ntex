use std::{fmt, future::Future, time::Instant};

pub use crate::{handle::JoinHandle, rt::Handle, rt::Runtime};

#[derive(Debug, Copy, Clone)]
pub struct JoinError;

impl fmt::Display for JoinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "JoinError")
    }
}

impl std::error::Error for JoinError {}

/// Returns the start of the current runtime turn.
///
/// Tasks polled in the same turn observe the same instant, see
/// [`Runtime::now`]. Returns [`Instant::now()`] outside of a runtime.
#[inline]
pub fn now() -> Instant {
    crate::rt::Runtime::current_now()
}

pub fn spawn<F>(fut: F) -> JoinHandle<F::Output>
where
    F: Future + 'static,
{
    let fut = crate::task::wrap(fut);
    crate::rt::Runtime::with_current(|rt| rt.spawn(fut))
}
