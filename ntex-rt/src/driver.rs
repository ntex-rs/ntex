//! Interfaces between the runtime and its event-loop driver.
use std::{any::Any, cell::RefCell, fmt, io, panic, pin::Pin, rc::Rc};

use crate::rt::Runtime;

/// A boxed, pinned future passed to a [`Runner`].
///
/// The future may contain thread-local state and is not required to be `Send`.
pub type BlockFuture = Pin<Box<dyn Future<Output = ()>>>;

/// Operating-system I/O mechanism used by a driver.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DriverType {
    /// Readiness-based polling, such as epoll or kqueue.
    Poll,
    /// Linux io-uring.
    IoUring,
    /// Windows I/O completion ports.
    Iocp,
}

impl DriverType {
    /// Returns the driver name: `"polling"`, `"io-uring"`, or `"iocp"`.
    pub const fn name(&self) -> &'static str {
        match self {
            DriverType::Poll => "polling",
            DriverType::IoUring => "io-uring",
            DriverType::Iocp => "iocp",
        }
    }

    /// Returns whether this is a readiness-based polling driver.
    pub const fn is_polling(&self) -> bool {
        matches!(self, &DriverType::Poll)
    }
}

/// Creates and drives an event loop for a system or arbiter thread.
///
/// A system shares its runner between arbiters. Implementations must support
/// calls from multiple threads, while each supplied future stays on its calling
/// thread.
pub trait Runner: Send + Sync + 'static {
    /// Runs the future to completion.
    ///
    /// Return `Ok(())` only after the future completes. A panic of the event loop
    /// may be returned as an error containing its panic payload or may unwind;
    /// the system performs cleanup and resumes the panic in either case.
    fn block_on(&self, fut: BlockFuture) -> Result<(), Box<dyn Any + Send>>;
}

/// Drives I/O events and scheduled tasks for the native [`Runtime`].
///
/// Unlike [`Runner`], a driver belongs to a single runtime thread.
pub trait Driver: 'static {
    /// Returns a thread-safe notifier for this driver's event loop.
    fn handle(&self) -> Box<dyn Notify>;

    /// Drives the runtime until [`Runtime::poll`] returns [`PollResult::Ready`].
    ///
    /// Called by [`Runtime::block_on`] with `rt` active on the current thread.
    /// Process external events between polls, without blocking when the result
    /// is [`PollResult::PollAgain`]. Call [`Runtime::awake`] after waiting for
    /// events, before dispatching events that may wake tasks.
    ///
    /// Returns an error if the event loop cannot continue.
    fn run(&self, rt: &Runtime) -> io::Result<()>;

    /// Releases driver-owned state when requested by its owner.
    ///
    /// The default implementation does nothing. [`Runtime::block_on`] does not
    /// call this hook.
    fn clear(&self) {}
}

/// Outcome of one call to [`Runtime::poll`].
///
/// Part of the native driver interface: [`Driver::run`] implementations inspect
/// this result to decide whether to stop, wait for events, or run another
/// nonblocking iteration.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PollResult {
    /// The future supplied to [`Runtime::block_on`] has completed; stop driving.
    Ready,
    /// No work is immediately runnable; the driver may wait for events.
    Pending,
    /// Work remains; process external events without blocking, then poll again.
    PollAgain,
}

/// Wakes a runtime driver, retaining the wakeup if it has not blocked yet.
///
/// Internal scheduler notifications retry [`io::ErrorKind::Interrupted`] and
/// log and abort the process on any other error or if `notify` panics, since
/// queued work may otherwise remain stranded. Explicit notifications through
/// [`Runtime::handle`] return errors and propagate panics to their caller.
pub trait Notify: Send + Sync + fmt::Debug + 'static {
    /// Wakes the driver or records a notification for its next wait.
    ///
    /// Notifications may be coalesced, but a notification racing with entry
    /// into a blocking wait must not be lost. Returns an error if notification
    /// fails.
    fn notify(&self) -> io::Result<()>;
}

/// Executes a Windows system call and converts its failure sentinel to an I/O error.
///
/// The `BOOL` and `SOCKET` forms check for zero and nonzero respectively;
/// `HANDLE` checks for `INVALID_HANDLE_VALUE`. The general form accepts an
/// explicit comparison, as in `syscall!(expression, == 0)`.
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

/// Executes a Unix system call and converts a `-1` result to the last I/O error.
///
/// The `break` form retries interruptions and returns [`std::task::Poll::Pending`]
/// for `WouldBlock` or `EINPROGRESS`, otherwise returning a ready result.
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
#[path = "../tests/unit/driver.rs"]
mod tests;
