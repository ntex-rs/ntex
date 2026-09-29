use std::sync::{Arc, atomic::AtomicBool, atomic::Ordering};
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

/// Process and application signals delivered to the runtime.
#[derive(Clone, Debug)]
pub enum Signal {
    /// SIGHUP
    Hup,
    /// SIGINT
    Int,
    /// SIGTERM
    Term,
    /// SIGQUIT
    Quit,
    /// Application panic
    Panic(PanicSource),
}

/// Source of a panic signal.
#[derive(Clone, Debug)]
pub enum PanicSource {
    /// A `SIGSEGV` or `SIGABRT` signal was received.
    Sig(&'static str),
    /// An application panic and its captured backtrace.
    App(Arc<str>, Backtrace),
}

/// Registers interest in the next batch of signals.
///
/// The returned one-shot receiver handles one notification. Call this function
/// again after each notification to continue receiving signals.
pub fn signal() -> oneshot::AsyncReceiver<Arc<[Signal]>> {
    let (tx, rx) = oneshot::async_channel();
    System::current().handle().spawn(async move {
        HANDLERS.with(|handlers| {
            handlers.borrow_mut().push(tx);
        });
    });

    rx
}

/// Returns whether signal handling is enabled.
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
        sys.handle().spawn(signals(rx));
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
        sys.handle().spawn(async move {
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
                signal::SaFlags::SA_NODEFER | signal::SaFlags::SA_ONSTACK,
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

        let usr2 = unsafe { register(SIGUSR2, || crate::system::sig_usr2()) };
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

#[cfg(target_family = "unix")]
extern "C" fn sig_segv(v: i32) {
    use nix::sys::signal::{self, SaFlags, SigAction, SigHandler, SigSet};

    let (sig, prev, name) = if v == libc::SIGABRT {
        (signal::SIGABRT, &PREV_SIGABRT, "SIGABRT")
    } else {
        (signal::SIGSEGV, &PREV_SIGSEGV, "SIGSEGV")
    };
    eprintln!("{name} Received:\n{:?}", backtrace::Backtrace::new());
    // best effort, waking the system is not signal-safe
    if let Some(mut sigs) = SIGS.try_lock() {
        sigs.push(Signal::Panic(PanicSource::Sig(name)));
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
}

/// Installs the panic hook, once per process.
///
/// The previous hook is called first. Panics are delivered as
/// `Signal::Panic` only while signal handling is enabled.
pub(crate) fn enable_panic_handling() {
    static ONCE: std::sync::Once = std::sync::Once::new();

    ONCE.call_once(|| {
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

            handle_signal(Signal::Panic(PanicSource::App(info, bt)));
        }));
    });
}

#[cfg(all(test, any(target_family = "windows", target_os = "linux")))]
mod tests {
    use crate::testing::TestRunner;

    use super::*;

    // signal handling is global
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    async fn recv(rx: oneshot::AsyncReceiver<Arc<[Signal]>>) -> Option<Arc<[Signal]>> {
        let mut rx = std::pin::pin!(rx);
        let mut timeout =
            std::pin::pin!(futures_timer::Delay::new(std::time::Duration::from_secs(5)));
        poll_fn(|cx| {
            if let Poll::Ready(res) = rx.as_mut().poll(cx) {
                Poll::Ready(res.ok())
            } else if timeout.as_mut().poll(cx).is_ready() {
                Poll::Ready(None)
            } else {
                Poll::Pending
            }
        })
        .await
    }

    #[test]
    fn panic_hook_chains_and_follows_signals() {
        use std::sync::atomic::AtomicUsize;

        static CALLS: AtomicUsize = AtomicUsize::new(0);

        let _lock = LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let prev = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            CALLS.fetch_add(1, Ordering::Relaxed);
            prev(info);
        }));
        enable_panic_handling();
        // installed once
        enable_panic_handling();

        // previous hook is called, nothing is queued without signal handling
        let _ = panic::catch_unwind(|| panic!("no signals"));
        assert_eq!(CALLS.load(Ordering::Relaxed), 1);
        assert!(SIGS.lock().is_empty());

        System::new("test", TestRunner).block_on(async {
            let sys = System::current();
            sys.enable_signals();

            let rx = signal();
            // let the registration task run
            futures_timer::Delay::new(std::time::Duration::from_millis(50)).await;
            let _ = panic::catch_unwind(|| panic!("boom"));

            let sigs = recv(rx).await.expect("panic delivered");
            assert!(
                matches!(&*sigs, [Signal::Panic(PanicSource::App(msg, _))] if &**msg == "boom")
            );
        });
        assert_eq!(CALLS.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn signals_released_when_system_stops() {
        let _lock = LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let first = System::new("first", TestRunner).block_on(async {
            let sys = System::current();
            sys.enable_signals();
            assert!(sys.signals());
            sys
        });
        assert!(!is_enabled());
        assert!(!first.signals());

        System::new("second", TestRunner).block_on(async {
            let sys = System::current();
            sys.enable_signals();
            assert!(sys.signals());
            assert!(is_enabled());

            // signals are handled by one system at a time
            let other = std::thread::spawn(|| {
                System::new("other", TestRunner).block_on(async {
                    let sys = System::current();
                    sys.enable_signals();
                    sys.signals()
                })
            })
            .join()
            .unwrap();
            assert!(!other);
            assert!(is_enabled());
        });
        assert!(!is_enabled());
    }

    #[test]
    fn signals_registered_by_one_system() {
        let _lock = LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let barrier = Arc::new(std::sync::Barrier::new(4));
        let handles: Vec<_> = (0..4)
            .map(|i| {
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    System::new(&format!("sys{i}"), TestRunner).block_on(async move {
                        let sys = System::current();
                        barrier.wait();
                        sys.enable_signals();
                        let enabled = sys.signals();
                        // keep signals registered until all systems tried
                        barrier.wait();
                        enabled
                    })
                })
            })
            .collect();
        let enabled = handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|enabled| *enabled)
            .count();
        assert_eq!(enabled, 1);
        assert!(!is_enabled());
    }

    #[test]
    fn signals_queued_from_many_threads() {
        let _lock = LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let handles: Vec<_> = (0..4)
            .map(|_| {
                std::thread::spawn(|| {
                    for _ in 0..25 {
                        handle_signal(Signal::Hup);
                    }
                })
            })
            .collect();
        let mut received = 0;
        while received < 100 && !handles.iter().all(std::thread::JoinHandle::is_finished) {
            received += std::mem::take(&mut *SIGS.lock()).len();
        }
        for h in handles {
            h.join().unwrap();
        }
        received += std::mem::take(&mut *SIGS.lock()).len();
        assert_eq!(received, 100);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn os_signal_delivered() {
        use std::time::Duration;

        let _lock = LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        System::new("test", TestRunner).block_on(async {
            let sys = System::current();
            sys.enable_signals();
            assert!(sys.signals());

            let rx = signal();
            // let the registration task run
            futures_timer::Delay::new(Duration::from_millis(50)).await;
            unsafe { libc::kill(libc::getpid(), libc::SIGHUP) };

            let sigs = recv(rx).await.expect("signal delivered");
            assert!(matches!(&*sigs, [Signal::Hup]));
        });
        assert!(!is_enabled());
    }

    #[cfg(target_family = "windows")]
    #[test]
    fn reenable_signals() {
        let _lock = LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        System::new("test", TestRunner).block_on(async {
            let sys = System::current();
            sys.enable_signals();
            sys.disable_signals();
            sys.enable_signals();
            assert!(sys.signals());
            sys.disable_signals();
            assert!(!sys.signals());
        });
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn segv_terminates_process() {
        use std::os::unix::process::ExitStatusExt;
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};

        if std::env::var_os("NTEX_SEGV_CHILD").is_some() {
            System::new("test", TestRunner).block_on(async {
                System::current().enable_signals();
                unsafe {
                    let page = libc::mmap(
                        std::ptr::null_mut(),
                        4096,
                        libc::PROT_NONE,
                        libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                        -1,
                        0,
                    );
                    assert_ne!(page, libc::MAP_FAILED);
                    page.cast::<u8>().write_volatile(1);
                }
            });
            return;
        }

        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "signals::tests::segv_terminates_process"])
            .env("NTEX_SEGV_CHILD", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let start = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if start.elapsed() > Duration::from_secs(30) {
                let _ = child.kill();
                panic!("process did not terminate after SIGSEGV");
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        assert_eq!(status.signal(), Some(libc::SIGSEGV));
    }
}
