//! The async operations.
//!
//! Types in this mod represents the low-level operations passed to kernel.
//! The operation itself doesn't perform anything.
//! You need to pass them to [`crate::Proactor`], and poll the driver.

use std::{io, marker::PhantomPinned, mem::ManuallyDrop, net::Shutdown};

#[cfg(unix)]
pub use super::sys::op::{CreateSocket, Interest};

use super::OwnedFd;

pub trait Handler {
    /// Submitted interest
    fn readable(&mut self, id: usize);

    /// Submitted interest
    fn writable(&mut self, id: usize);

    /// Operation submission has failed
    fn error(&mut self, id: usize, err: io::Error);

    /// All events are processed, process all updates
    fn commit(&mut self);
}

/// Spawn a blocking function in the thread pool.
pub struct Asyncify<F, D> {
    pub(crate) f: Option<F>,
    pub(crate) data: Option<D>,
    _p: PhantomPinned,
}

impl<F, D> Asyncify<F, D> {
    /// Create [`Asyncify`].
    pub fn new(f: F) -> Self {
        Self {
            f: Some(f),
            data: None,
            _p: PhantomPinned,
        }
    }

    pub fn into_inner(mut self) -> D {
        self.data.take().expect("the data should not be None")
    }
}

/// Shutdown a socket.
pub struct ShutdownSocket<S> {
    pub(crate) fd: S,
    pub(crate) how: Shutdown,
}

impl<S> ShutdownSocket<S> {
    /// Create [`ShutdownSocket`].
    pub fn new(fd: S, how: Shutdown) -> Self {
        Self { fd, how }
    }
}

/// Close socket fd.
pub struct CloseSocket {
    pub(crate) fd: ManuallyDrop<OwnedFd>,
}

impl CloseSocket {
    /// Create [`CloseSocket`].
    pub fn new(fd: OwnedFd) -> Self {
        Self {
            fd: ManuallyDrop::new(fd),
        }
    }
}
