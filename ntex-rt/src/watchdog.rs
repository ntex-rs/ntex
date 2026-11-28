//! Stalled arbiter detection.
use std::sync::atomic::{AtomicBool, Ordering, fence};
use std::sync::{Arc, OnceLock, Weak};
use std::thread::{self, Thread};
use std::{panic::Location, time::Duration, time::Instant};

use parking_lot::Mutex;

use crate::arbiter::Arbiter;
#[cfg(target_os = "linux")]
use crate::capture::CAPTURE;
use crate::metrics::Stall;
use crate::system::{System, SystemInner};

type LatencyCallback =
    Arc<dyn Fn(ntex_error::Backtrace, Option<&'static Location<'static>>) + Send + Sync>;

static ARB_CB: Mutex<Option<LatencyCallback>> = Mutex::new(None);

pub(crate) fn set_latency_callback(f: LatencyCallback) {
    *ARB_CB.lock() = Some(f);
}

/// Clone the callback so it runs without the lock and survives replacement.
fn latency_callback() -> Option<LatencyCallback> {
    ARB_CB.lock().clone()
}

/// Watchdog thread state shared with the arbiters.
///
/// The thread wakes when a stall can start, at most once per latency
/// interval. It parks while every arbiter is idle past its sample deadline,
/// an arbiter in that state expires the deadline at its next turn and
/// notifies the watchdog.
#[derive(Debug, Default)]
pub(crate) struct Watchdog {
    thread: OnceLock<Thread>,
    parked: AtomicBool,
    stop: AtomicBool,
    // checks made by the thread
    #[cfg(test)]
    passes: std::sync::atomic::AtomicUsize,
}

impl Watchdog {
    /// Wakes the thread if it is parked.
    ///
    /// State the watchdog checks before it parks must be written before the call.
    pub(crate) fn notify(&self) {
        // pairs with the fence in `park`, either the thread sees the state
        // written before this call or this call sees it parked
        fence(Ordering::SeqCst);
        if self.parked.load(Ordering::Acquire)
            && let Some(thread) = self.thread.get()
        {
            thread.unpark();
        }
    }

    /// Stops the thread.
    pub(crate) fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.get() {
            thread.unpark();
        }
    }

    fn is_stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// Sleeps for `timeout` unless the thread is stopped.
    fn sleep(&self, timeout: Duration) {
        let end = Instant::now() + timeout;
        while !self.is_stopped() {
            let Some(timeout) = end.checked_duration_since(Instant::now()) else {
                break;
            };
            thread::park_timeout(timeout);
        }
    }

    /// Parks the thread unless `idle` returns `false` after the thread is
    /// marked parked.
    fn park(&self, idle: impl FnOnce() -> bool) {
        self.parked.store(true, Ordering::Release);
        fence(Ordering::SeqCst);
        if !self.is_stopped() && idle() {
            thread::park();
        }
        self.parked.store(false, Ordering::Relaxed);
    }
}

/// Starts a thread that detects stalled arbiters.
pub(crate) fn start(sys: &System) {
    let interval = sys.0.config.latency_interval;
    if interval == 0 {
        return;
    }
    let interval = Duration::from_millis(interval as u64);
    let threshold = Duration::from_millis(sys.0.config.ping_threshold as u64);
    let name = format!("{}:watchdog", sys.0.config.name);
    let sys = Arc::downgrade(&sys.0);

    if let Err(e) = thread::Builder::new()
        .name(name)
        .spawn(move || watchdog(&sys, interval, threshold))
    {
        log::error!("Cannot start arbiter watchdog: {e}");
    }
}

fn watchdog(sys: &Weak<SystemInner>, interval: Duration, threshold: Duration) {
    let Some(wd) = sys.upgrade().map(|sys| sys.watchdog.clone()) else {
        return;
    };
    let _ = wd.thread.set(thread::current());

    let mut wait = interval;
    loop {
        wd.sleep(wait);
        if wd.is_stopped() {
            return;
        }
        let Some(sys) = sys.upgrade() else {
            return;
        };
        #[cfg(test)]
        wd.passes.fetch_add(1, Ordering::Relaxed);

        let now = Instant::now();
        let mut idle = true;
        // an arbiter that starts sampling now cannot stall earlier
        let mut next = now + threshold;
        for arb in sys.arbiters() {
            if !arb.is_running() {
                continue;
            }
            let turn = arb.handle().current_turn();
            check(&arb, now, threshold, turn);
            if let Some(at) = arb.0.latency.stall_at(now, threshold, turn) {
                next = next.min(at);
            }
            idle = idle && arb.0.latency.is_dormant(now, turn);
        }
        // wake when a stall can start, at most once per interval
        wait = next.saturating_duration_since(now).max(interval);
        if idle {
            wait = interval;
            // arbiters registered or woken after the first pass notify the
            // thread, re-check them once it is marked parked
            wd.park(|| {
                let now = Instant::now();
                let idle = sys.arbiters().iter().all(|arb| {
                    !arb.is_running() || {
                        let turn = arb.handle().current_turn();
                        arb.0.latency.is_dormant(now, turn)
                    }
                });
                drop(sys);
                idle
            });
        }
    }
}

fn check(arb: &Arbiter, now: Instant, threshold: Duration, turn: Option<Duration>) {
    match arb.0.latency.check(now, threshold, turn) {
        Stall::Started(age) => {
            let task = arb.handle().current_task();
            if let Some(task) = task {
                log::error!(
                    "Arbiter {}({:?}) is stalled, no progress for {age:?}, running task spawned at {task}",
                    arb.name(),
                    arb.id()
                );
            } else {
                log::error!(
                    "Arbiter {}({:?}) is stalled, no progress for {age:?}",
                    arb.name(),
                    arb.id()
                );
            }
            if let Some(bt) = capture_backtrace(arb) {
                // the backtrace belongs to the task only if it still runs
                let task = task.filter(|t| {
                    arb.handle()
                        .current_task()
                        .is_some_and(|cur| std::ptr::eq(*t, cur))
                });
                report(arb, age, bt, task);
            }
        }
        Stall::Recovered(time) => {
            log::warn!(
                "Arbiter {}({:?}) recovered after {time:?} stall",
                arb.name(),
                arb.id()
            );
        }
        Stall::None => {}
    }
}

fn report(
    arb: &Arbiter,
    age: Duration,
    bt: ntex_error::BacktraceRaw,
    task: Option<&'static Location<'static>>,
) {
    let bt = ntex_error::Backtrace::from(bt);
    if let Some(f) = latency_callback() {
        f(bt, task);
    } else {
        bt.resolver().resolve();
        if let Some(task) = task {
            log::error!(
                "Arbiter {}({:?}) made no progress for {age:?}, running task spawned at {task}.\n{bt:?}",
                arb.name(),
                arb.id()
            );
        } else {
            log::error!(
                "Arbiter {}({:?}) made no progress for {age:?}.\n{bt:?}",
                arb.name(),
                arb.id()
            );
        }
    }
}

#[cfg(target_os = "linux")]
/// Sends `SIGUSR2` to the stalled arbiter thread to capture a backtrace.
fn capture_backtrace(arb: &Arbiter) -> Option<ntex_error::BacktraceRaw> {
    const SPIN: Duration = Duration::from_micros(100);

    if !crate::signals::is_enabled() {
        return None;
    }
    let tid = arb.tid();
    if !CAPTURE.arm(tid) {
        return None;
    }
    // SAFETY: `tgkill` takes scalar IDs and a signal number, targeting an arbiter in this process.
    let result = unsafe { libc::syscall(libc::SYS_tgkill, libc::getpid(), tid, libc::SIGUSR2) };

    let mut bt = None;
    if result == -1 {
        log::error!(
            "Unable to send SIGUSR2 to arbiter {}({:?}): {}",
            arb.name(),
            arb.id(),
            std::io::Error::last_os_error()
        );
    } else {
        for _ in 0..1000 {
            thread::sleep(SPIN);
            bt = CAPTURE.take(tid);
            if bt.is_some() {
                break;
            }
        }
    }
    CAPTURE.disarm(tid);
    bt
}

#[track_caller]
#[cfg(target_family = "unix")]
pub(crate) fn sig_usr2() {
    #[cfg(target_os = "linux")]
    #[allow(clippy::cast_possible_truncation)]
    {
        // SAFETY: `SYS_gettid` takes no pointer arguments and is safe to invoke from a signal handler.
        let tid = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
        CAPTURE.capture(tid, Location::caller().file());
    }
}

#[cfg(windows)]
/// Suspends the stalled arbiter thread and walks its stack.
fn capture_backtrace(arb: &Arbiter) -> Option<ntex_error::BacktraceRaw> {
    let thread = arb.thread()?;
    let mut ips = [0usize; 80];
    match thread.walk(&mut ips) {
        Ok(0) => None,
        Ok(len) => Some(ntex_error::BacktraceRaw::from_ips(&ips[..len], file!())),
        Err(e) => {
            log::error!(
                "Unable to capture backtrace of arbiter {}({:?}): {e}",
                arb.name(),
                arb.id()
            );
            None
        }
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
fn capture_backtrace(_: &Arbiter) -> Option<ntex_error::BacktraceRaw> {
    None
}

#[cfg(windows)]
pub(crate) use self::windows::ThreadHandle;

#[cfg(windows)]
mod windows {
    use std::io;

    use windows_sys::Win32::Foundation::{
        CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE,
    };
    use windows_sys::Win32::System::Diagnostics::Debug::CONTEXT;
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentThread, GetCurrentThreadStackLimits, ResumeThread,
        SuspendThread,
    };

    /// Handle of an arbiter thread, used to capture its backtrace.
    pub(crate) struct ThreadHandle {
        handle: HANDLE,
        stack: (usize, usize),
    }

    // SAFETY: The duplicated handle is a process-wide kernel reference with no thread-local ownership.
    unsafe impl Send for ThreadHandle {}
    // SAFETY: Shared access only uses kernel thread APIs and immutable stack bounds; closing requires ownership.
    unsafe impl Sync for ThreadHandle {}

    impl Drop for ThreadHandle {
        fn drop(&mut self) {
            // SAFETY: `current` obtains one owned duplicated handle, closed only by this destructor.
            unsafe { CloseHandle(self.handle) };
        }
    }

    #[repr(C, align(16))]
    struct AlignedContext(CONTEXT);

    impl ThreadHandle {
        /// Returns the handle of the current thread.
        pub(crate) fn current() -> Option<Self> {
            let mut handle = std::ptr::null_mut();
            let mut stack = (0, 0);
            // SAFETY: Current-process/thread pseudo-handles are valid, and all output pointers are writable locals.
            unsafe {
                let process = GetCurrentProcess();
                if DuplicateHandle(
                    process,
                    GetCurrentThread(),
                    process,
                    &raw mut handle,
                    0,
                    0,
                    DUPLICATE_SAME_ACCESS,
                ) == 0
                {
                    return None;
                }
                GetCurrentThreadStackLimits(&raw mut stack.0, &raw mut stack.1);
            }
            Some(Self { handle, stack })
        }

        /// Suspends the thread and writes the instruction pointers of its
        /// stack frames to `ips`, innermost first.
        ///
        /// The suspended thread can hold any lock, including the heap lock,
        /// so nothing may allocate or lock until the thread is resumed.
        pub(crate) fn walk(&self, ips: &mut [usize]) -> io::Result<usize> {
            if !cfg!(any(target_arch = "x86_64", target_arch = "aarch64")) {
                return Ok(0);
            }
            // SAFETY: The owned handle is valid; `get_context` observes suspension before walking the stack.
            unsafe {
                if SuspendThread(self.handle) == u32::MAX {
                    return Err(io::Error::last_os_error());
                }
                let mut ctx: AlignedContext = std::mem::zeroed();
                let result = get_context(self.handle, &mut ctx.0)
                    .map(|()| unwind(&mut ctx.0, self.stack, ips));
                ResumeThread(self.handle);
                result
            }
        }
    }

    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    unsafe fn get_context(handle: HANDLE, ctx: &mut CONTEXT) -> io::Result<()> {
        use windows_sys::Win32::System::Diagnostics::Debug::GetThreadContext;
        #[cfg(target_arch = "x86_64")]
        use windows_sys::Win32::System::Diagnostics::Debug::{
            CONTEXT_CONTROL_AMD64 as CONTROL, CONTEXT_INTEGER_AMD64 as INTEGER,
        };
        #[cfg(target_arch = "aarch64")]
        use windows_sys::Win32::System::Diagnostics::Debug::{
            CONTEXT_CONTROL_ARM64 as CONTROL, CONTEXT_INTEGER_ARM64 as INTEGER,
        };

        ctx.ContextFlags = CONTROL | INTEGER;
        // SAFETY: The caller suspended this valid thread handle; `ctx` is writable and suitably aligned.
        if unsafe { GetThreadContext(handle, ctx) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    unsafe fn get_context(_: HANDLE, _: &mut CONTEXT) -> io::Result<()> {
        Err(io::ErrorKind::Unsupported.into())
    }

    #[cfg(target_arch = "x86_64")]
    #[allow(clippy::cast_possible_truncation)]
    unsafe fn unwind(ctx: &mut CONTEXT, stack: (usize, usize), ips: &mut [usize]) -> usize {
        use windows_sys::Win32::System::Diagnostics::Debug::{
            RtlLookupFunctionEntry, RtlVirtualUnwind, UNW_FLAG_NHANDLER,
        };

        let mut len = 0;
        while len < ips.len() && ctx.Rip != 0 {
            ips[len] = ctx.Rip as usize;
            len += 1;

            let sp = ctx.Rsp as usize;
            if sp < stack.0 || sp >= stack.1 {
                break;
            }
            let mut base = 0;
            let entry =
                // SAFETY: The suspended thread supplies the instruction address, and `base` is a writable local.
                unsafe { RtlLookupFunctionEntry(ctx.Rip, &raw mut base, std::ptr::null_mut()) };
            if entry.is_null() {
                // leaf function, the return address is on top of the stack
                // SAFETY: The suspended stack pointer is in bounds and ABI-aligned for its return address.
                ctx.Rip = unsafe { *(sp as *const u64) };
                ctx.Rsp += 8;
            } else {
                let mut data = std::ptr::null_mut();
                let mut frame = 0;
                // SAFETY: The lookup supplied matching unwind metadata for this suspended, writable context.
                unsafe {
                    RtlVirtualUnwind(
                        UNW_FLAG_NHANDLER,
                        base,
                        ctx.Rip,
                        entry,
                        ctx,
                        &raw mut data,
                        &raw mut frame,
                        std::ptr::null_mut(),
                    );
                }
            }
            if (ctx.Rsp as usize) <= sp {
                break;
            }
        }
        len
    }

    #[cfg(target_arch = "aarch64")]
    #[allow(clippy::cast_possible_truncation)]
    unsafe fn unwind(ctx: &mut CONTEXT, stack: (usize, usize), ips: &mut [usize]) -> usize {
        use windows_sys::Win32::System::Diagnostics::Debug::{
            RtlLookupFunctionEntry, RtlVirtualUnwind, UNW_FLAG_NHANDLER,
        };

        let mut len = 0;
        while len < ips.len() && ctx.Pc != 0 {
            ips[len] = ctx.Pc as usize;
            len += 1;

            let (sp, pc) = (ctx.Sp as usize, ctx.Pc);
            if sp < stack.0 || sp >= stack.1 {
                break;
            }
            let mut base = 0;
            let entry =
                // SAFETY: The suspended thread supplies the instruction address, and `base` is a writable local.
                unsafe { RtlLookupFunctionEntry(pc as usize, &raw mut base, std::ptr::null_mut()) };
            if entry.is_null() {
                // leaf function, the return address is in the link register
                // SAFETY: `GetThreadContext` initialized the ARM64 integer-register union, including `X[30]`.
                ctx.Pc = unsafe { ctx.Anonymous.X[30] };
            } else {
                let mut data = std::ptr::null_mut();
                let mut frame = 0;
                // SAFETY: The lookup supplied matching unwind metadata for this suspended, writable context.
                unsafe {
                    RtlVirtualUnwind(
                        UNW_FLAG_NHANDLER,
                        base,
                        pc as usize,
                        entry,
                        ctx,
                        &raw mut data,
                        &raw mut frame,
                        std::ptr::null_mut(),
                    );
                }
            }
            if ctx.Pc == pc && (ctx.Sp as usize) <= sp {
                break;
            }
        }
        len
    }

    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    unsafe fn unwind(_: &mut CONTEXT, _: (usize, usize), _: &mut [usize]) -> usize {
        0
    }
}

#[cfg(test)]
#[path = "../tests/unit/watchdog.rs"]
mod tests;
