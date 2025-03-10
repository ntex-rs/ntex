use std::{io, marker::Send, os::fd::FromRawFd, os::fd::RawFd, pin::Pin, task::Poll};

pub use crate::driver::unix::op::*;

use super::{AsRawFd, Decision, OpCode};
use crate::{driver::op::*, syscall};

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
