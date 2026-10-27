//! Process-signal subscriptions and optional application-panic notifications.
//!
//! Enable handling with [`System::enable_signals`] or
//! [`Builder::signals`](crate::Builder::signals). Only one system handles
//! process signals at a time; subscriptions belong to the current system.
//! Unix delivers hangup, interrupt, termination, and quit signals, plus
//! best-effort fatal-signal notifications. Windows delivers Ctrl-C as
//! [`Signal::Int`]. Application panics additionally require
//! [`Builder::panic_handling`](crate::Builder::panic_handling).
use std::sync::{Arc, OnceLock, atomic::AtomicBool, atomic::Ordering, mpsc};
use std::{cell::RefCell, future::poll_fn, panic, task::Poll};

use atomic_waker::AtomicWaker;
use ntex_error::Backtrace;
use parking_lot::{Mutex, MutexGuard};

use crate::System;

thread_local! {
    static STOP: RefCell<Option<oneshot::Sender<()>>> = const { RefCell::new(None) };
    static HANDLERS: RefCell<Vec<oneshot::Sender<Arc<[Signal]>>>> = RefCell::default();
}

static CUR_SYS: Mutex<Option<System>> = Mutex::new(None);
// Mirrors `CUR_SYS`, the panic hook must not lock it.
static ENABLED: AtomicBool = AtomicBool::new(false);
static SIGS: Mutex<Vec<Signal>> = Mutex::new(Vec::new());
static HND_WAKER: AtomicWaker = AtomicWaker::new();
// The panic hook runs on the panicking thread, some runtimes (compio) abort
// the process if a task is woken there. Panics are woken from a dedicated thread.
static PANIC_WAKER: OnceLock<mpsc::SyncSender<()>> = OnceLock::new();

/// Process and application signals delivered to the runtime.
#[derive(Clone, Debug)]
pub enum Signal {
    /// A Unix hangup signal (`SIGHUP`).
    Hup,
    /// An interrupt signal (`SIGINT` on Unix, Ctrl-C on Windows).
    Int,
    /// A Unix termination signal (`SIGTERM`).
    Term,
    /// A Unix quit signal (`SIGQUIT`).
    Quit,
    /// An application panic or a best-effort fatal-signal notification.
    Panic(PanicSource),
}

/// Source of a panic signal.
#[derive(Clone, Debug)]
pub enum PanicSource {
    /// A `SIGSEGV` or `SIGABRT` signal was received.
    ///
    /// Delivery is best effort; a fatal fault can terminate the process before
    /// the runtime or server handles this notification.
    Sig(&'static str),
    /// An application panic message and its captured, unresolved backtrace.
    App(Arc<str>, Backtrace),
}

/// Registers interest in the next batch of signals.
///
/// The returned one-shot receiver handles one notification. Call this function
/// again after each notification to continue receiving signals.
///
/// Registration is scheduled on the current system's primary arbiter. Each
/// batch is broadcast to the subscriptions registered when it is dispatched;
/// this is not a replayable signal history. This function does not enable signal
/// handling, and subscriptions in a system that does not own signal handling
/// do not receive process signals.
///
/// # Panics
///
/// Panics if no system is installed on the current thread.
pub fn signal() -> oneshot::AsyncReceiver<Arc<[Signal]>> {
    let (tx, rx) = oneshot::async_channel();
    System::current().handle().spawn_detached(async move {
        HANDLERS.with(|handlers| {
            handlers.borrow_mut().push(tx);
        });
    });

    rx
}

/// Returns whether any system currently owns process signal handling.
///
/// Unlike [`System::signals`], this is process-wide and does not require a
/// current system.
pub fn is_enabled() -> bool {
    CUR_SYS.lock().is_some()
}

type Registration = MutexGuard<'static, Option<System>>;

/// Registers the system as the signal handler.
///
/// Returns the lock guard, so handlers are installed before other systems
/// can register or unregister.
fn register_system(sys: &System) -> Option<Registration> {
    let mut cur = CUR_SYS.lock();
    if cur.is_some() {
        None
    } else {
        *cur = Some(sys.clone());
        ENABLED.store(true, Ordering::Release);

        let (tx, rx) = oneshot::async_channel();
        sys.handle().spawn_detached(signals(rx));
        STOP.with(|stop| {
            *stop.borrow_mut() = Some(tx);
        });
        Some(cur)
    }
}

/// Unregisters the system if it handles signals.
///
/// Returns the lock guard, so handlers are removed before other systems
/// can register.
fn unregister_system(sys: &System) -> Option<Registration> {
    let mut cur = CUR_SYS.lock();
    if cur.as_ref().is_some_and(|cur| cur.id() == sys.id()) {
        cur.take();
        ENABLED.store(false, Ordering::Release);
        sys.handle().spawn_detached(async move {
            STOP.with(|stop| {
                if let Some(tx) = stop.borrow_mut().take() {
                    let _ = tx.send(());
                }
            });
        });
        Some(cur)
    } else {
        None
    }
}

/// Queue signal and wake the system.
///
/// Must not be called from a signal handler.
fn handle_signal(sig: Signal) {
    SIGS.lock().push(sig);
    HND_WAKER.wake();
}

#[cfg(target_family = "unix")]
/// Signal delivery thread handle and `SIGUSR2` handler id.
static SIG_HANDLERS: Mutex<(
    Option<signal_hook::iterator::Handle>,
    Option<signal_hook::SigId>,
)> = Mutex::new((None, None));

#[cfg(target_family = "unix")]
/// Register signal handler.
///
/// Returns `false` if signals are handled by another system.
pub(crate) fn start(sys: &System) -> bool {
    static ONCE: std::sync::Once = std::sync::Once::new();

    if let Some(_registration) = register_system(sys) {
        use nix::sys::signal;
        use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGQUIT, SIGTERM, SIGUSR2};
        use signal_hook::{iterator::Signals, low_level::register};

        ONCE.call_once(|| {
            if let Err(e) = start_fatal_signal_thread() {
                log::error!("Cannot start fatal signal handling thread: {e:?}");
            }

            // Use u128 for alignment.
            let buf = Vec::leak(vec![0u128; 4096]);
            let stack = libc::stack_t {
                ss_sp: buf.as_ptr() as *mut libc::c_void,
                ss_flags: 0,
                ss_size: std::mem::size_of_val(buf),
            };
            let mut old = libc::stack_t {
                ss_sp: std::ptr::null_mut(),
                ss_flags: 0,
                ss_size: 0,
            };
            let result = unsafe { libc::sigaltstack(&raw const stack, &raw mut old) };
            if result != 0 {
                log::error!("Cannot set signal stack");
            }

            let sig_action = signal::SigAction::new(
                signal::SigHandler::Handler(sig_segv),
                signal::SaFlags::SA_ONSTACK,
                signal::SigSet::empty(),
            );
            unsafe {
                match signal::sigaction(signal::SIGSEGV, &sig_action) {
                    Ok(prev) => _ = PREV_SIGSEGV.set(prev),
                    Err(_) => log::error!("Cannot install signal handler for SIGSEGV"),
                }
                match signal::sigaction(signal::SIGABRT, &sig_action) {
                    Ok(prev) => _ = PREV_SIGABRT.set(prev),
                    Err(_) => log::error!("Cannot install signal handler for SIGABRT"),
                }
            }
        });

        // signal handlers only write to a self-pipe, signals are
        // dispatched from a regular thread
        let handle = match Signals::new([SIGHUP, SIGINT, SIGTERM, SIGQUIT]) {
            Ok(mut signals) => {
                let handle = signals.handle();
                let result = std::thread::Builder::new()
                    .name("ntex-rt:signals".to_string())
                    .spawn(move || {
                        for sig in signals.forever() {
                            handle_signal(match sig {
                                SIGHUP => Signal::Hup,
                                SIGINT => Signal::Int,
                                SIGTERM => Signal::Term,
                                SIGQUIT => Signal::Quit,
                                _ => continue,
                            });
                        }
                    });
                match result {
                    Ok(_) => Some(handle),
                    Err(e) => {
                        log::error!("Cannot start signal handling thread: {e:?}");
                        None
                    }
                }
            }
            Err(e) => {
                log::error!("Cannot install signal handlers: {e:?}");
                None
            }
        };

        let usr2 = unsafe { register(SIGUSR2, || crate::watchdog::sig_usr2()) };
        if usr2.is_err() {
            log::error!("Cannot install signal handler for SIGUSR2");
        }
        *SIG_HANDLERS.lock() = (handle, usr2.ok());
        true
    } else {
        false
    }
}

#[cfg(target_family = "unix")]
/// Unregister signal handler.
pub(crate) fn stop(sys: &System) {
    if let Some(_registration) = unregister_system(sys) {
        let (handle, usr2) = std::mem::take(&mut *SIG_HANDLERS.lock());
        if let Some(handle) = handle {
            // the thread exits and unregisters handlers
            handle.close();
        }
        if let Some(usr2) = usr2 {
            signal_hook::low_level::unregister(usr2);
        }
    }
}

#[cfg(target_family = "windows")]
static CTRLC_ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(target_family = "windows")]
/// Register signal handler.
///
/// Signals are handled by oneshots, you have to re-register
/// after each signal. Returns `false` if signals are handled by another system.
pub(crate) fn start(sys: &System) -> bool {
    use std::sync::atomic::Ordering;
    static ONCE: std::sync::Once = std::sync::Once::new();

    if let Some(_registration) = register_system(sys) {
        // the handler cannot be removed, it ignores signals while disabled
        ONCE.call_once(|| {
            let result = ctrlc::set_handler(|| {
                if CTRLC_ENABLED.load(Ordering::Acquire) {
                    handle_signal(Signal::Int);
                }
            });
            if let Err(e) = result {
                log::error!("Cannot install Ctrl-C handler: {e:?}");
            }
        });
        CTRLC_ENABLED.store(true, Ordering::Release);
        true
    } else {
        false
    }
}

#[cfg(target_family = "windows")]
/// Unregister signal handler.
pub(crate) fn stop(sys: &System) {
    if let Some(_registration) = unregister_system(sys) {
        CTRLC_ENABLED.store(false, std::sync::atomic::Ordering::Release);
        log::info!("Signals handling is disabled");
    }
}

async fn signals(rx: oneshot::AsyncReceiver<()>) {
    let mut rx = std::pin::pin!(rx);

    poll_fn(|cx| {
        if rx.as_mut().poll(cx).is_ready() {
            Poll::Ready(())
        } else {
            HND_WAKER.register(cx.waker());

            let sigs = std::mem::take(&mut *SIGS.lock());
            if !sigs.is_empty() {
                let sigs: Arc<[Signal]> = Arc::from(sigs);

                HANDLERS.with(|handlers| {
                    for tx in handlers.borrow_mut().drain(..) {
                        let _ = tx.send(sigs.clone());
                    }
                });
            }

            Poll::Pending
        }
    })
    .await;
}

#[cfg(target_family = "unix")]
static PREV_SIGSEGV: std::sync::OnceLock<nix::sys::signal::SigAction> = std::sync::OnceLock::new();
#[cfg(target_family = "unix")]
static PREV_SIGABRT: std::sync::OnceLock<nix::sys::signal::SigAction> = std::sync::OnceLock::new();

// Both ends stay open for the process lifetime: a handler must never write to a
// recycled descriptor or raise SIGPIPE if the delivery thread exits.
#[cfg(target_family = "unix")]
static FATAL_PIPE: OnceLock<(
    libc::pid_t,
    std::os::unix::net::UnixStream,
    std::os::unix::net::UnixStream,
)> = OnceLock::new();

#[cfg(target_family = "unix")]
fn start_fatal_signal_thread() -> std::io::Result<()> {
    use std::{io::Read, os::unix::net::UnixStream};

    let (read, write) = UnixStream::pair()?;
    write.set_nonblocking(true)?;
    let (_, read, _) = FATAL_PIPE.get_or_init(|| (unsafe { libc::getpid() }, read, write));
    std::thread::Builder::new()
        .name("ntex-rt:crashes".to_string())
        .spawn(move || {
            let mut read = read;
            let mut byte = [0];
            while read.read_exact(&mut byte).is_ok() {
                let name = match byte[0] {
                    b'A' => "SIGABRT",
                    b'S' => "SIGSEGV",
                    _ => continue,
                };
                if ENABLED.load(Ordering::Acquire) {
                    handle_signal(Signal::Panic(PanicSource::Sig(name)));
                }
            }
        })?;
    Ok(())
}

#[cfg(target_family = "unix")]
extern "C" fn sig_segv(v: i32) {
    use std::os::fd::AsRawFd;

    use nix::errno::Errno;
    use nix::sys::signal::{self, SaFlags, SigAction, SigHandler, SigSet};

    let errno = Errno::last_raw();
    let (sig, prev, byte) = if v == libc::SIGABRT {
        (signal::SIGABRT, &PREV_SIGABRT, b"A")
    } else {
        (signal::SIGSEGV, &PREV_SIGSEGV, b"S")
    };
    if let Some((pid, _, write)) = FATAL_PIPE.get() {
        unsafe {
            // A forked child must not shut down its parent's server.
            if libc::getpid() == *pid {
                let _ = libc::write(write.as_raw_fd(), byte.as_ptr().cast(), 1);
            }
        }
    }

    // restore the previous handler, it handles the signal raised again by
    // the faulting instruction or by `abort()`, otherwise the process never exits
    let prev = prev
        .get()
        .copied()
        .unwrap_or_else(|| SigAction::new(SigHandler::SigDfl, SaFlags::empty(), SigSet::empty()));
    unsafe {
        let _ = signal::sigaction(sig, &prev);
    }
    Errno::set_raw(errno);
}

/// Installs the panic hook, once per process.
///
/// The previous hook is called first. Panics are delivered as
/// `Signal::Panic` only while signal handling is enabled.
pub(crate) fn enable_panic_handling() {
    static ONCE: std::sync::Once = std::sync::Once::new();

    ONCE.call_once(|| {
        let (tx, rx) = mpsc::sync_channel::<()>(1);
        let result = std::thread::Builder::new()
            .name("ntex-rt:panics".to_string())
            .spawn(move || {
                for () in rx {
                    HND_WAKER.wake();
                }
            });
        match result {
            Ok(_) => _ = PANIC_WAKER.set(tx),
            Err(e) => log::error!("Cannot start panic handling thread: {e:?}"),
        }

        let prev = panic::take_hook();
        panic::set_hook(Box::new(move |panic_info| {
            prev(panic_info);

            if !ENABLED.load(Ordering::Acquire) {
                return;
            }

            let info: Arc<str> = if let Some(s) = panic_info.payload().downcast_ref::<&str>() {
                Arc::from(s.to_string())
            } else if let Some(s) = panic_info.payload().downcast_ref::<String>() {
                Arc::from(s.clone())
            } else {
                Arc::from("panic")
            };
            let bt = if let Some(loc) = panic_info.location() {
                let s = Box::new(loc.file().to_string());
                let filename = Box::leak(s);
                Backtrace::with_filename(filename)
            } else {
                Backtrace::new(panic::Location::caller())
            };

            SIGS.lock().push(Signal::Panic(PanicSource::App(info, bt)));
            if let Some(tx) = PANIC_WAKER.get() {
                // a pending wake covers this panic too
                let _ = tx.try_send(());
            }
        }));
    });
}

#[cfg(all(test, any(target_family = "windows", target_family = "unix")))]
#[path = "../tests/unit/signals.rs"]
mod tests;
