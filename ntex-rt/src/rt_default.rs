use std::{fmt, future::Future};

pub use crate::{handle::JoinHandle, rt::Handle, rt::Runtime};

#[derive(Debug, Copy, Clone)]
pub struct JoinError;

impl fmt::Display for JoinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "JoinError")
    }
}

impl std::error::Error for JoinError {}

pub fn spawn<F>(fut: F) -> JoinHandle<F::Output>
where
    F: Future + 'static,
{
    let fut = crate::task::wrap(fut);
    crate::rt::Runtime::with_current(|rt| rt.spawn(fut))
}
