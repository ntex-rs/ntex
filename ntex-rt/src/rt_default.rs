use std::{fmt, future::Future, future::poll_fn};

pub use crate::rt::{Handle, Runtime};

#[derive(Debug, Copy, Clone)]
pub struct JoinError;

impl fmt::Display for JoinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "JoinError")
    }
}

impl std::error::Error for JoinError {}

pub fn spawn<F>(fut: F) -> crate::handle::JoinHandle<F::Output>
where
    F: Future + 'static,
{
    if let Some(mut data) = crate::task::Data::load() {
        crate::rt::Runtime::with_current(|rt| {
            rt.spawn(async move {
                let mut f = std::pin::pin!(fut);
                poll_fn(|cx| data.run(|| f.as_mut().poll(cx))).await
            })
        })
    } else {
        crate::rt::Runtime::with_current(|rt| rt.spawn(fut))
    }
}
