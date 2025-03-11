use std::{io, os::fd::AsRawFd, pin::Pin};

pub use crate::driver::unix::op::*;

use super::OpCode;
use crate::{driver::op::*, syscall};

pub trait Handler {
    /// Operation is completed
    fn completed(&mut self, user_data: usize, flags: u32, result: io::Result<i32>);

    fn canceled(&mut self, user_data: usize);
}

impl<D, F> OpCode for Asyncify<F, D>
where
    D: Send + 'static,
    F: (FnOnce() -> (io::Result<usize>, D)) + Send + 'static,
{
    fn name(&self) -> &'static str {
        "Asyncify"
    }

    fn call_blocking(self: Pin<&mut Self>) -> std::io::Result<usize> {
        // Safety: self won't be moved
        let this = unsafe { self.get_unchecked_mut() };
        let f = this
            .f
            .take()
            .expect("the operate method could only be called once");
        let (res, data) = f();
        this.data = Some(data);
        res
    }
}

impl OpCode for CreateSocket {
    fn name(&self) -> &'static str {
        "CreateSocket"
    }

    fn call_blocking(self: Pin<&mut Self>) -> io::Result<usize> {
        Ok(syscall!(libc::socket(self.domain, self.socket_type, self.protocol))? as _)
    }
}

impl<S: AsRawFd> OpCode for ShutdownSocket<S> {
    fn name(&self) -> &'static str {
        "ShutdownSocket"
    }

    fn call_blocking(self: Pin<&mut Self>) -> io::Result<usize> {
        Ok(syscall!(libc::shutdown(self.fd.as_raw_fd(), self.how()))? as _)
    }
}
