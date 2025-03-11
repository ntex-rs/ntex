use std::{io, marker::Send, mem, os::fd::FromRawFd, os::fd::RawFd, pin::Pin, task::Poll};

pub use crate::driver::unix::op::*;

use super::{AsRawFd, Decision, OpCode, OwnedFd};
use crate::{driver::op::*, syscall};

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

impl<D, F> OpCode for Asyncify<F, D>
where
    D: Send + 'static,
    F: (FnOnce() -> (io::Result<usize>, D)) + Send + 'static,
{
    fn pre_submit(self: Pin<&mut Self>) -> io::Result<Decision> {
        Ok(Decision::Blocking)
    }

    fn operate(self: Pin<&mut Self>) -> Poll<io::Result<usize>> {
        // Safety: self won't be moved
        let this = unsafe { self.get_unchecked_mut() };
        let f = this
            .f
            .take()
            .expect("the operate method could only be called once");
        let (res, data) = f();
        this.data = Some(data);
        Poll::Ready(res)
    }
}

/// Close socket fd.
pub struct CloseSocket {
    pub(crate) fd: mem::ManuallyDrop<OwnedFd>,
}

impl CloseSocket {
    /// Create [`CloseSocket`].
    pub fn new(fd: OwnedFd) -> Self {
        Self {
            fd: mem::ManuallyDrop::new(fd),
        }
    }
}

impl OpCode for CreateSocket {
    fn pre_submit(self: Pin<&mut Self>) -> io::Result<Decision> {
        Ok(Decision::Blocking)
    }

    fn operate(self: Pin<&mut Self>) -> Poll<io::Result<usize>> {
        Poll::Ready(Ok(
            syscall!(libc::socket(self.domain, self.socket_type, self.protocol))? as _,
        ))
    }
}

impl<S: AsRawFd> OpCode for ShutdownSocket<S> {
    fn pre_submit(self: Pin<&mut Self>) -> io::Result<Decision> {
        Ok(Decision::Blocking)
    }

    fn operate(self: Pin<&mut Self>) -> Poll<io::Result<usize>> {
        Poll::Ready(Ok(
            syscall!(libc::shutdown(self.fd.as_raw_fd(), self.how()))? as _,
        ))
    }
}

impl CloseSocket {
    pub fn from_raw_fd(fd: RawFd) -> Self {
        Self::new(unsafe { FromRawFd::from_raw_fd(fd) })
    }
}

impl OpCode for CloseSocket {
    fn pre_submit(self: Pin<&mut Self>) -> io::Result<Decision> {
        Ok(Decision::Blocking)
    }

    fn operate(self: Pin<&mut Self>) -> Poll<io::Result<usize>> {
        Poll::Ready(Ok(syscall!(libc::close(self.fd.as_raw_fd()))? as _))
    }
}
