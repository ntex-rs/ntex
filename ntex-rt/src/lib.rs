//! A runtime implementation that runs everything on the current thread.
#![allow(clippy::return_self_not_must_use)]
use std::future::Future;

mod arbiter;
mod builder;
mod system;

pub use self::arbiter::Arbiter;
pub use self::builder::{Builder, SystemRunner};
pub use self::system::System;

#[cfg(feature = "tokio")]
mod tokio {
    /// Runs the provided future, blocking the current thread until the future
    /// completes.
    pub fn block_on<F: Future<Output = ()>>(fut: F) {
        let rt = runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .unwrap();
        LocalSet::new().block_on(&rt, fut);
    }

    /// Spawn a future on the current thread. This does not create a new Arbiter
    /// or Arbiter address, it is simply a helper for spawning futures on the current
    /// thread.
    ///
    /// # Panics
    ///
    /// This function panics if ntex system is not running.
    #[inline]
    pub fn spawn<F>(f: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: Future + 'static,
    {
        tokio::task::spawn_local(f)
    }

    /// Executes a future on the current thread. This does not create a new Arbiter
    /// or Arbiter address, it is simply a helper for executing futures on the current
    /// thread.
    ///
    /// # Panics
    ///
    /// This function panics if ntex system is not running.
    #[inline]
    pub fn spawn_fn<F, R>(f: F) -> tokio::task::JoinHandle<R::Output>
    where
        F: FnOnce() -> R + 'static,
        R: Future + 'static,
    {
        spawn(async move {
            let r = lazy(|_| f()).await;
            r.await
        })
    }
}

#[cfg(feature = "async-std")]
mod asyncstd {
    /// Runs the provided future, blocking the current thread until the future
    /// completes.
    pub fn block_on<F: Future<Output = ()>>(fut: F) {
        async_std::task::block_on(f);
    }

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
        JoinHandle {
            fut: async_std::task::spawn_local(f),
        }
    }

    /// Executes a future on the current thread. This does not create a new Arbiter
    /// or Arbiter address, it is simply a helper for executing futures on the current
    /// thread.
    ///
    /// # Panics
    ///
    /// This function panics if ntex system is not running.
    #[inline]
    pub fn spawn_fn<F, R>(f: F) -> JoinHandle<R::Output>
    where
        F: FnOnce() -> R + 'static,
        R: Future + 'static,
    {
        spawn(async move {
            let r = lazy(|_| f()).await;
            r.await
        })
    }

    #[derive(Debug, Copy, Clone, derive_more::Display)]
    pub struct JoinError;
    impl std::error::Error for JoinError {}

    pub struct JoinHandle<T> {
        fut: async_std::task::JoinHandle<T>,
    }

    impl<T> Future for JoinHandle<T> {
        type Output = Result<T, JoinError>;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            Poll::Ready(Ok(ready!(Pin::new(&mut self.fut).poll(cx))))
        }
    }
}

#[cfg(feature = "tokio")]
pub use self::tokio::*;

#[cfg(all(not(feature = "tokio"), feature = "async-std"))]
pub use self::asyncstd::*;

/// Runs the provided future, blocking the current thread until the future
/// completes.
#[cfg(all(not(feature = "tokio"), not(feature = "async-std")))]
pub fn block_on<F: Future<Output = ()>>(_: F) {
    panic!("async runtime is not configured");
}

#[cfg(all(not(feature = "tokio"), not(feature = "async-std")))]
pub fn spawn<F>(_: F) -> std::pin::Pin<Box<dyn std::future::Future<Output = F::Output>>>
where
    F: std::future::Future + 'static,
{
    unimplemented!()
}
