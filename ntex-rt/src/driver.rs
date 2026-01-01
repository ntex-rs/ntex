//! The platform-specified driver.
use std::{fmt, io, pin::Pin};

use crate::rt::Runtime;

pub type BlockFuture = Pin<Box<dyn Future<Output = ()> + 'static>>;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DriverType {
    Poll,
    IoUring,
}

impl DriverType {
    pub const fn name(&self) -> &'static str {
        match self {
            DriverType::Poll => "polling",
            DriverType::IoUring => "io-uring",
        }
    }

    pub const fn is_polling(&self) -> bool {
        matches!(self, &DriverType::Poll)
    }
}

pub trait Runner: Send + Sync + 'static {
    fn block_on(&self, fut: BlockFuture);
}

pub trait Driver: 'static {
    fn handle(&self) -> Box<dyn Notify>;

    fn run(&self, rt: &Runtime) -> io::Result<()>;

    fn clear(&self);
}

pub enum PollResult {
    Ready,
    Pending,
    PollAgain,
}

pub trait Notify: Send + Sync + fmt::Debug + 'static {
    fn notify(&self) -> io::Result<()>;
}

#[cfg(windows)]
#[macro_export]
macro_rules! syscall {
    (BOOL, $e:expr) => {
        $crate::syscall!($e, == 0)
    };
    (SOCKET, $e:expr) => {
        $crate::syscall!($e, != 0)
    };
    (HANDLE, $e:expr) => {
        $crate::syscall!($e, == ::windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE)
    };
    ($e:expr, $op: tt $rhs: expr) => {{
        #[allow(unused_unsafe)]
        let res = unsafe { $e };
        if res $op $rhs {
            Err(::std::io::Error::last_os_error())
        } else {
            Ok(res)
        }
    }};
}

/// Helper macro to execute a system call
#[cfg(unix)]
#[macro_export]
macro_rules! syscall {
    (break $e:expr) => {
        loop {
            match $crate::syscall!($e) {
                Ok(fd) => break ::std::task::Poll::Ready(Ok(fd as usize)),
                Err(e) if e.kind() == ::std::io::ErrorKind::WouldBlock || e.raw_os_error() == Some(::libc::EINPROGRESS)
                    => break ::std::task::Poll::Pending,
                Err(e) if e.kind() == ::std::io::ErrorKind::Interrupted => {},
                Err(e) => break ::std::task::Poll::Ready(Err(e)),
            }
        }
    };
    ($e:expr, $f:ident($fd:expr)) => {
        match $crate::syscall!(break $e) {
            ::std::task::Poll::Pending => Ok($crate::sys::Decision::$f($fd)),
            ::std::task::Poll::Ready(Ok(res)) => Ok($crate::sys::Decision::Completed(res)),
            ::std::task::Poll::Ready(Err(e)) => Err(e),
        }
    };
    ($e:expr) => {{
        #[allow(unused_unsafe)]
        let res = unsafe { $e };
        if res == -1 {
            Err(::std::io::Error::last_os_error())
        } else {
            Ok(res)
        }
    }};
}
