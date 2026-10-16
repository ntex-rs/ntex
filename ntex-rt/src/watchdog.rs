//! Stalled arbiter detection.
use std::sync::{Arc, Weak, atomic::AtomicBool, atomic::Ordering};
use std::{panic::Location, thread, time::Duration, time::Instant};

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

/// Starts a thread that detects stalled arbiters.
///
/// Returns the stop flag of the thread.
pub(crate) fn start(sys: &System) -> Option<Arc<AtomicBool>> {
    let interval = sys.0.config.latency_interval;
    if interval == 0 {
        return None;
    }
    let interval = Duration::from_millis(interval as u64);
    let threshold = Duration::from_millis(sys.0.config.ping_threshold as u64);
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let sys = Arc::downgrade(&sys.0);

    let name = format!("{}:watchdog", sys.upgrade()?.config.name);
    match thread::Builder::new()
        .name(name)
        .spawn(move || watchdog(&sys, &stop2, interval, threshold))
    {
        Ok(_) => Some(stop),
        Err(e) => {
            log::error!("Cannot start arbiter watchdog: {e}");
            None
        }
    }
}

fn watchdog(sys: &Weak<SystemInner>, stop: &AtomicBool, interval: Duration, threshold: Duration) {
    loop {
        thread::sleep(interval);
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let Some(arbiters) = sys.upgrade().map(|sys| sys.arbiters()) else {
            return;
        };

        let now = Instant::now();
        for arb in arbiters {
            if !arb.is_running() {
                continue;
            }
            match arb.0.latency.check(now, threshold) {
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
                    if let Some(bt) = capture_backtrace(&arb) {
                        // the backtrace belongs to the task only if it still runs
                        let task = task.filter(|t| {
                            arb.handle()
                                .current_task()
                                .is_some_and(|cur| std::ptr::eq(*t, cur))
                        });
                        report(&arb, age, bt, task);
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

    // The handle is a process-wide kernel object reference.
    unsafe impl Send for ThreadHandle {}
    unsafe impl Sync for ThreadHandle {}

    impl Drop for ThreadHandle {
        fn drop(&mut self) {
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
        // also waits until the thread is actually suspended
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
                unsafe { RtlLookupFunctionEntry(ctx.Rip, &raw mut base, std::ptr::null_mut()) };
            if entry.is_null() {
                // leaf function, the return address is on top of the stack
                ctx.Rip = unsafe { *(sp as *const u64) };
                ctx.Rsp += 8;
            } else {
                let mut data = std::ptr::null_mut();
                let mut frame = 0;
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
                unsafe { RtlLookupFunctionEntry(pc as usize, &raw mut base, std::ptr::null_mut()) };
            if entry.is_null() {
                // leaf function, the return address is in the link register
                ctx.Pc = unsafe { ctx.Anonymous.X[30] };
            } else {
                let mut data = std::ptr::null_mut();
                let mut frame = 0;
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
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::{panic::Location, sync::Arc};

    use futures_timer::Delay;

    use super::*;
    use crate::testing::TestRunner;

    /// Serializes tests that stall arbiters or set the global latency callback.
    static CALLBACK: Mutex<()> = Mutex::new(());

    #[test]
    fn stall_detection() {
        let _guard = CALLBACK.lock();
        // parked threads on CI VMs (macOS) can wake up tens of ms late
        System::build()
            .name("test")
            .latency_interval(10)
            .ping_threshold(200)
            .build(TestRunner)
            .block_on(async {
                let mut arb = Arbiter::new();
                let (tx, rx) = oneshot::channel();
                let line = line!() + 1;
                arb.handle().spawn(async move {
                    std::thread::sleep(Duration::from_millis(800));
                    let _ = tx.send(());
                });

                let stalled = || arb.latency().unwrap().stalled;
                for _ in 0..400 {
                    if stalled() {
                        break;
                    }
                    Delay::new(Duration::from_millis(5)).await;
                }
                assert!(stalled());
                // the watchdog names the blocking task
                let task = arb.handle().current_task();
                if cfg!(any(feature = "tokio", feature = "compio")) {
                    assert_eq!(task, None);
                } else {
                    let task = task.unwrap();
                    assert_eq!((task.file(), task.line()), (file!(), line));
                }
                rx.await.unwrap();

                for _ in 0..400 {
                    if !stalled() {
                        break;
                    }
                    Delay::new(Duration::from_millis(5)).await;
                }
                let m = arb.latency().unwrap();
                assert!(!m.stalled);
                assert_eq!(m.stalls, 1);
                assert!(m.stall_duration >= Duration::from_millis(500), "{m:?}");

                // the stall is attributed to the blocked arbiter only
                let sys = System::current().arbiter().latency().unwrap();
                assert!(!sys.stalled);
                assert!(sys.stall_duration < m.stall_duration, "{sys:?}");

                arb.stop();
                arb.join().unwrap();
            });
    }

    #[test]
    fn latency_callback_replaced_while_running() {
        let _guard = CALLBACK.lock();
        let calls = Arc::new(AtomicUsize::new(0));
        let data = Arc::new(vec![1u8; 64]);
        let calls2 = calls.clone();
        System::set_latency_callback(move |_, _| {
            // replace itself, then use captured state
            System::set_latency_callback(|_, _| {});
            assert_eq!(data.len(), 64);
            calls2.fetch_add(1, Ordering::Relaxed);
        });

        latency_callback().unwrap()(ntex_error::Backtrace::new(Location::caller()), None);
        latency_callback().unwrap()(ntex_error::Backtrace::new(Location::caller()), None);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        *ARB_CB.lock() = None;
    }

    #[cfg(all(windows, any(target_arch = "x86_64", target_arch = "aarch64")))]
    // Spins instead of blocking, a blocking call can be a tail call that
    // leaves no frame of this function. Exported, so the symbol is resolved
    // without debug info.
    #[inline(never)]
    #[unsafe(no_mangle)]
    extern "C" fn ntex_rt_stalled_arbiter_fn(stop: &AtomicBool) {
        while !stop.load(Ordering::Acquire) {
            std::hint::spin_loop();
        }
    }

    #[cfg(all(windows, any(target_arch = "x86_64", target_arch = "aarch64")))]
    #[test]
    fn stall_backtrace() {
        let _guard = CALLBACK.lock();
        let (bt_tx, bt_rx) = std::sync::mpsc::channel();
        let bt_tx = Mutex::new(bt_tx);
        System::set_latency_callback(move |bt, task| {
            let _ = bt_tx.lock().send((format!("{:?}", bt.resolve()), task));
        });

        System::build()
            .name("test")
            .latency_interval(10)
            .ping_threshold(50)
            .build(TestRunner)
            .block_on(async move {
                let mut arb = Arbiter::new();
                let stop = Arc::new(AtomicBool::new(false));
                let stop2 = stop.clone();
                let line = line!() + 1;
                let done = arb.handle().spawn(async move {
                    ntex_rt_stalled_arbiter_fn(&stop2);
                });

                // other tests can stall their arbiters too
                let mut found = None;
                for _ in 0..2000 {
                    if let Ok((bt, task)) = bt_rx.try_recv()
                        && bt.contains("ntex_rt_stalled_arbiter_fn")
                    {
                        found = Some((bt, task));
                        break;
                    }
                    Delay::new(Duration::from_millis(5)).await;
                }
                stop.store(true, Ordering::Release);
                done.await.unwrap();
                let (bt, task) = found.expect("backtrace of the stalled arbiter");
                assert!(bt.contains("stall_backtrace"), "{bt}");
                if cfg!(any(feature = "tokio", feature = "compio")) {
                    assert_eq!(task, None);
                } else {
                    let task = task.unwrap();
                    assert_eq!((task.file(), task.line()), (file!(), line));
                }

                let recovered = || !arb.latency().unwrap().stalled;
                for _ in 0..400 {
                    if recovered() {
                        break;
                    }
                    Delay::new(Duration::from_millis(5)).await;
                }
                assert!(recovered());
                arb.stop();
                arb.join().unwrap();
            });
        *ARB_CB.lock() = None;
    }
}
