#![allow(static_mut_refs)]
use std::{cell::RefCell, future::poll_fn, panic, sync::Arc, task::Poll};

use atomic_waker::AtomicWaker;
use ntex_error::Backtrace;

use crate::System;

thread_local! {
    static STOP: RefCell<Option<oneshot::Sender<()>>> = const { RefCell::new(None) };
    static HANDLERS: RefCell<Vec<oneshot::Sender<Arc<[Signal]>>>> = RefCell::default();
}

static mut CUR_SYS: Option<System> = None;
static mut SIGS: [Option<Signal>; 10] = [const { None }; 10];
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
    unsafe { CUR_SYS.is_some() }
}

fn register_system(sys: &System) -> bool {
    unsafe {
        if CUR_SYS.is_some() {
            false
        } else {
            CUR_SYS = Some(sys.clone());

            let (tx, rx) = oneshot::async_channel();
            sys.handle().spawn(signals(rx));
            STOP.with(|stop| {
                *stop.borrow_mut() = Some(tx);
            });
            true
        }
    }
}

fn unregister_system(sys: &System) -> bool {
    unsafe {
        if let Some(cur) = CUR_SYS.take() {
            if cur.id() == sys.id() {
                sys.handle().spawn(async move {
                    STOP.with(|stop| {
                        if let Some(tx) = stop.borrow_mut().take() {
                            let _ = tx.send(());
                        }
                    });
                });
                true
            } else {
                CUR_SYS = Some(cur);
                false
            }
        } else {
            false
        }
    }
}

fn handle_signal(sig: Signal) {
    unsafe {
        for s in &mut SIGS {
            if s.is_none() {
                *s = Some(sig);
                break;
            }
        }
        HND_WAKER.wake();
    }
}

#[cfg(target_family = "unix")]
static mut SIG_HANDLERS: [Option<signal_hook::SigId>; 10] = [None; 10];

#[cfg(target_family = "unix")]
/// Register signal handler.
pub(crate) fn start(sys: &System) {
    static ONCE: std::sync::Once = std::sync::Once::new();

    if register_system(sys) {
        use nix::sys::signal;
        use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGQUIT, SIGTERM, SIGUSR2};
        use signal_hook::low_level::register;

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

        for (idx, s, sig) in [
            (0, SIGHUP, Signal::Hup),
            (1, SIGINT, Signal::Int),
            (2, SIGTERM, Signal::Term),
            (3, SIGQUIT, Signal::Quit),
        ] {
            unsafe {
                let sig2 = sig.clone();
                match register(s, move || handle_signal(sig.clone())) {
                    Ok(s) => SIG_HANDLERS[idx] = Some(s),
                    Err(e) => {
                        log::error!("Cannot install signal handler for {sig2:?} with {e:?}");
                    }
                }
            }
        }

        unsafe {
            match register(SIGUSR2, || crate::system::sig_usr2()) {
                Ok(s) => SIG_HANDLERS[5] = Some(s),
                Err(_) => log::error!("Cannot install signal handler for SIGUSR2"),
            }
        }
    }
}

#[cfg(target_family = "unix")]
/// Unregister signal handler.
pub(crate) fn stop(sys: &System) {
    if unregister_system(sys) {
        use signal_hook::low_level::unregister;

        unsafe {
            for sig in &mut SIG_HANDLERS {
                if let Some(s) = sig.take() {
                    let _ = unregister(s);
                }
            }
        }
    }
}

#[cfg(target_family = "windows")]
static CTRLC_ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(target_family = "windows")]
/// Register signal handler.
///
/// Signals are handled by oneshots, you have to re-register
/// after each signal.
pub(crate) fn start(sys: &System) {
    use std::sync::atomic::Ordering;
    static ONCE: std::sync::Once = std::sync::Once::new();

    if register_system(sys) {
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
    }
}

#[cfg(target_family = "windows")]
/// Unregister signal handler.
pub(crate) fn stop(sys: &System) {
    if unregister_system(sys) {
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

            let mut sigs = Vec::new();
            unsafe {
                for sig in &mut SIGS {
                    if let Some(sig) = sig.take() {
                        sigs.push(sig);
                    }
                }
            }
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
    handle_signal(Signal::Panic(PanicSource::Sig(name)));

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

pub(crate) fn enable_panic_handling() {
    panic::set_hook(Box::new(|panic_info| {
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
}

#[cfg(all(test, any(target_family = "windows", target_os = "linux")))]
mod tests {
    use std::{any::Any, io};

    use crate::{BlockFuture, Driver, Notify, PollResult, Runner, Runtime};

    use super::*;

    #[derive(Debug)]
    struct NoopNotify;

    impl Notify for NoopNotify {
        fn notify(&self) -> io::Result<()> {
            Ok(())
        }
    }

    struct BusyDriver;

    impl Driver for BusyDriver {
        fn handle(&self) -> Box<dyn crate::Notify> {
            Box::new(NoopNotify)
        }

        fn run(&self, rt: &Runtime) -> io::Result<()> {
            while rt.poll() != PollResult::Ready {}
            Ok(())
        }
    }

    struct TestRunner;

    impl Runner for TestRunner {
        fn block_on(&self, fut: BlockFuture) -> Result<(), Box<dyn Any + Send>> {
            Runtime::new(Box::new(NoopNotify)).block_on(fut, &BusyDriver);
            Ok(())
        }
    }

    #[cfg(target_family = "windows")]
    #[test]
    fn reenable_signals() {
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
