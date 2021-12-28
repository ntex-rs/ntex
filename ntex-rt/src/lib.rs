//! A runtime implementation that runs everything on the current thread.
#![allow(clippy::return_self_not_must_use)]
use std::{future::Future, pin::Pin};

mod arbiter;
mod builder;
mod system;

pub use self::arbiter::Arbiter;
pub use self::builder::{Builder, SystemRunner};
pub use self::system::System;

#[cfg(feature = "tokio")]
mod tokio;
#[cfg(feature = "tokio")]
pub use self::tokio::*;

#[cfg(feature = "async-std")]
mod asyncstd;
#[cfg(all(not(feature = "tokio"), feature = "async-std"))]
pub use self::asyncstd::*;

pub trait Runtime {
    /// Spawn a future onto the single-threaded runtime.
    fn spawn(&self, future: Pin<Box<dyn Future<Output = ()>>>);

    /// Runs the provided future, blocking the current thread until the future
    /// completes.
    fn block_on(&self, f: Pin<Box<dyn Future<Output = ()>>>);
}

/// Different types of process signals
#[derive(PartialEq, Clone, Copy, Debug)]
pub enum Signal {
    /// SIGHUP
    Hup,
    /// SIGINT
    Int,
    /// SIGTERM
    Term,
    /// SIGQUIT
    Quit,
}

#[cfg(all(not(feature = "tokio"), not(feature = "async-std")))]
pub fn create_runtime() -> Box<dyn Runtime> {
    unimplemented!()
}

#[cfg(all(not(feature = "tokio"), not(feature = "async-std")))]
pub fn spawn<F>(_: F) -> std::pin::Pin<Box<dyn std::future::Future<Output = F::Output>>>
where
    F: std::future::Future + 'static,
{
    unimplemented!()
}
