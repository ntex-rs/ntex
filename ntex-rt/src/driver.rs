//! The platform-specified driver.
use std::{any::Any, cell::RefCell, fmt, io, panic, pin::Pin, rc::Rc};

use crate::rt::Runtime;

pub type BlockFuture = Pin<Box<dyn Future<Output = ()>>>;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DriverType {
    Poll,
    IoUring,
    Iocp,
}

impl DriverType {
    pub const fn name(&self) -> &'static str {
        match self {
            DriverType::Poll => "polling",
            DriverType::IoUring => "io-uring",
            DriverType::Iocp => "iocp",
        }
    }

    pub const fn is_polling(&self) -> bool {
        matches!(self, &DriverType::Poll)
    }
}

pub trait Runner: Send + Sync + 'static {
    /// Runs the future to completion.
    ///
    /// A panic of the event loop may be returned as an error or may unwind,
    /// both are handled the same way.
    fn block_on(&self, fut: BlockFuture) -> Result<(), Box<dyn Any + Send>>;
}

pub trait Driver: 'static {
    fn handle(&self) -> Box<dyn Notify>;

    fn run(&self, rt: &Runtime) -> io::Result<()>;

    fn clear(&self) {}
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PollResult {
    Ready,
    Pending,
    PollAgain,
}

/// Wakes a runtime driver, retaining the wakeup if it has not blocked yet.
///
/// Internal scheduler notifications retry [`io::ErrorKind::Interrupted`] and
/// log and abort the process on any other error or if `notify` panics, since
/// queued work may otherwise remain stranded. Explicit notifications through
/// [`Runtime::handle`] return errors and propagate panics to their caller.
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

/// Execute a future with custom `block_on` method and wait for result
pub(super) fn block_on<F, R>(run: &dyn Runner, fut: F) -> Result<R, Box<dyn Any + Send>>
where
    F: Future<Output = R> + 'static,
    R: 'static,
{
    // run loop
    let result = Rc::new(RefCell::new(None));
    let result_inner = result.clone();

    ntex_error::set_backtrace_start_alt("ntex/ntex-rt/src/driver.rs", 0);
    // finalizes the thread's arbiter once its event loop has exited,
    // including when the runner returns an error or a shutdown callback panics
    let finalize = FinalizeArbiter;
    let fut = Box::pin(async move {
        let r = fut.await;
        *result_inner.borrow_mut() = Some(r);
    });
    // a panic escaping the runner is returned like a runner error, so the
    // caller cleans up the same way for unwinding and catching runners
    let res =
        panic::catch_unwind(panic::AssertUnwindSafe(|| run.block_on(fut))).unwrap_or_else(Err);
    if let Err(e) = res {
        // the event loop panicked, a panic of `on_stop` aborts the process
        crate::abort_on_panic(|| drop(finalize));
        return Err(e);
    }

    crate::arbiter::run_shutdown_callbacks();
    unsafe {
        crate::remove_all_items();
    }
    Ok(result.borrow_mut().take().unwrap())
}

struct FinalizeArbiter;

impl Drop for FinalizeArbiter {
    fn drop(&mut self) {
        if let Some(arb) = crate::Arbiter::try_current() {
            arb.finalize();
        }
    }
}

pub(super) fn block_on_panic<F, R>(run: &dyn Runner, fut: F) -> R
where
    F: Future<Output = R> + 'static,
    R: 'static,
{
    match block_on(run, fut) {
        Ok(v) => v,
        Err(e) => panic::resume_unwind(e),
    }
}

#[cfg(test)]
mod tests {
    use super::DriverType;

    #[test]
    fn driver_type() {
        assert_eq!(DriverType::Poll.name(), "polling");
        assert_eq!(DriverType::IoUring.name(), "io-uring");
        assert_eq!(DriverType::Iocp.name(), "iocp");
        assert!(DriverType::Poll.is_polling());
        assert!(!DriverType::IoUring.is_polling());
        assert!(!DriverType::Iocp.is_polling());
    }
}
