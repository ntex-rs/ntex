//! A runtime implementation that runs everything on the current thread.
#![deny(clippy::pedantic)]
#![allow(
    clippy::missing_fields_in_debug,
    clippy::must_use_candidate,
    clippy::missing_errors_doc,
    clippy::let_underscore_future
)]

mod arbiter;
mod builder;
mod driver;
mod handle;
mod pool;
mod rt;
mod system;
mod task;

pub use self::arbiter::Arbiter;
pub use self::builder::{Builder, SystemRunner};
pub use self::driver::{BlockFuture, Driver, DriverType, Notify, PollResult, Runner};
pub use self::pool::{BlockingError, BlockingResult};
pub use self::rt::{Runtime, RuntimeBuilder};
pub use self::system::{Id, PingRecord, System};
pub use self::task::{task_callbacks, task_opt_callbacks};

/// Spawns a blocking task in a new thread, and wait for it.
///
/// The task will not be cancelled even if the future is dropped.
pub fn spawn_blocking<F, R>(f: F) -> BlockingResult<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    System::current().spawn_blocking(f)
}

#[cfg(feature = "tokio")]
mod tokio {
    use std::future::{Future, poll_fn};
    pub use tok_io::task::{JoinError, JoinHandle};

    /// Spawn a future on the current thread. This does not create a new Arbiter
    /// or Arbiter address, it is simply a helper for spawning futures on the current
    /// thread.
    ///
    /// # Panics
    ///
    /// This function panics if ntex system is not running.
    #[inline]
    pub fn spawn<F>(f: F) -> tok_io::task::JoinHandle<F::Output>
    where
        F: Future + 'static,
    {
        if let Some(mut data) = crate::task::Data::load() {
            tok_io::task::spawn_local(async move {
                tok_io::pin!(f);
                poll_fn(|cx| data.run(|| f.as_mut().poll(cx))).await
            })
        } else {
            tok_io::task::spawn_local(f)
        }
    }

    #[derive(Clone, Debug)]
    /// Handle to the runtime.
    pub struct Handle(tok_io::runtime::Handle);

    impl Handle {
        #[inline]
        pub fn current() -> Self {
            Self(tok_io::runtime::Handle::current())
        }

        #[inline]
        /// Wake up runtime
        pub fn notify(&self) {}

        #[inline]
        /// Spawns a new asynchronous task, returning a [`Task`] for it.
        ///
        /// Spawning a task enables the task to execute concurrently to other tasks.
        /// There is no guarantee that a spawned task will execute to completion.
        pub fn spawn<F>(&self, future: F) -> tok_io::task::JoinHandle<F::Output>
        where
            F: Future + Send + 'static,
            F::Output: Send + 'static,
        {
            self.0.spawn(future)
        }
    }
}

#[cfg(feature = "compio")]
mod compio {
    use std::task::{Context, Poll, ready};
    use std::{fmt, future::Future, future::poll_fn, pin::Pin};

    /// Spawn a future on the current thread. This does not create a new Arbiter
    /// or Arbiter address, it is simply a helper for spawning futures on the current
    /// thread.
    ///
    /// # Panics
    ///
    /// This function panics if ntex system is not running.
    #[inline]
    pub fn spawn<F>(f: F) -> JoinHandle<F::Output>
    where
        F: Future + 'static,
    {
        let fut = if let Some(mut data) = crate::task::Data::load() {
            compio_runtime::spawn(async move {
                let mut f = std::pin::pin!(f);
                poll_fn(|cx| data.run(|| f.as_mut().poll(cx))).await
            })
        } else {
            compio_runtime::spawn(f)
        };

        JoinHandle {
            fut: Some(Either::Compio(fut)),
        }
    }

    #[derive(Debug, Copy, Clone)]
    pub struct JoinError;

    impl fmt::Display for JoinError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "JoinError")
        }
    }

    impl std::error::Error for JoinError {}

    #[derive(Debug)]
    enum Either<T> {
        Compio(compio_runtime::JoinHandle<T>),
        Spawn(oneshot::Receiver<T>),
    }

    #[derive(Debug)]
    pub struct JoinHandle<T> {
        fut: Option<Either<T>>,
    }

    impl<T> JoinHandle<T> {
        pub fn is_finished(&self) -> bool {
            match &self.fut {
                Some(Either::Compio(fut)) => fut.is_finished(),
                Some(Either::Spawn(fut)) => fut.is_closed(),
                None => true,
            }
        }
    }

    impl<T> Drop for JoinHandle<T> {
        fn drop(&mut self) {
            if let Some(Either::Compio(fut)) = self.fut.take() {
                fut.detach();
            }
        }
    }

    impl<T> Future for JoinHandle<T> {
        type Output = Result<T, JoinError>;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            Poll::Ready(match self.fut.as_mut() {
                Some(Either::Compio(fut)) => {
                    ready!(Pin::new(fut).poll(cx)).map_err(|_| JoinError)
                }
                Some(Either::Spawn(fut)) => {
                    ready!(Pin::new(fut).poll(cx)).map_err(|_| JoinError)
                }
                None => Err(JoinError),
            })
        }
    }

    #[derive(Clone, Debug)]
    pub struct Handle(crate::Arbiter);

    impl Handle {
        pub fn current() -> Self {
            Self(crate::Arbiter::current())
        }

        pub fn notify(&self) {}

        pub fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
        where
            F: Future + Send + 'static,
            F::Output: Send + 'static,
        {
            let (tx, rx) = oneshot::channel();
            self.0.spawn(async move {
                let result = future.await;
                let _ = tx.send(result);
            });
            JoinHandle {
                fut: Some(Either::Spawn(rx)),
            }
        }
    }
}

#[cfg(feature = "tokio")]
pub use self::tokio::*;

#[cfg(all(feature = "compio", not(feature = "tokio")))]
pub use self::compio::*;

pub mod default_runtime {
    use std::fmt;
    use std::future::{Future, poll_fn};

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
            Runtime::with_current(|rt| {
                rt.spawn(async move {
                    let mut f = std::pin::pin!(fut);
                    poll_fn(|cx| data.run(|| f.as_mut().poll(cx))).await
                })
            })
        } else {
            Runtime::with_current(|rt| rt.spawn(fut))
        }
    }
}

#[cfg(all(not(feature = "tokio"), not(feature = "compio")))]
pub use self::default_runtime::*;
