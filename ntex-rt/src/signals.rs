#![allow(static_mut_refs)]
use std::{cell::RefCell, future::poll_fn, sync::Arc, task::Poll};

use atomic_waker::AtomicWaker;

use crate::System;

thread_local! {
    static STOP: RefCell<Option<oneshot::Sender<()>>> = const { RefCell::new(None) };
    static HANDLERS: RefCell<Vec<oneshot::Sender<Arc<[Signal]>>>> = RefCell::default();
}

static mut CUR_SYS: Option<System> = None;
static mut SIGS: [Option<Signal>; 10] = [None; 10];
static HND_WAKER: AtomicWaker = AtomicWaker::new();

/// Different types of process signals
#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub enum Signal {
    /// SIGHUP
    Hup,
    /// SIGINT
    Int,
    /// SIGTERM
    Term,
    /// SIGQUIT
    Quit,
    /// SIGSEGV
    Segv,
}

/// Register signal handler.
///
/// Signals are handled by oneshots, you have to re-register
/// interest after each signal.
pub fn signal() -> oneshot::AsyncReceiver<Arc<[Signal]>> {
    let (tx, rx) = oneshot::async_channel();
    System::current().handle().spawn(async move {
        HANDLERS.with(|handlers| {
            handlers.borrow_mut().push(tx);
        });
    });

    rx
}

/// Check if signal handling is enabled.
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
                if signal::sigaction(signal::SIGSEGV, &sig_action).is_err() {
                    log::error!("Cannot install signal handler for SIGSEGV");
                }
                if signal::sigaction(signal::SIGABRT, &sig_action).is_err() {
                    log::error!("Cannot install signal handler for SIGABRT");
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
                match register(s, move || handle_signal(sig)) {
                    Ok(s) => SIG_HANDLERS[idx] = Some(s),
                    Err(e) => {
                        log::error!("Cannot install signal handler for {sig:?} with {e:?}");
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
/// Register signal handler.
///
/// Signals are handled by oneshots, you have to re-register
/// after each signal.
pub(crate) fn start(sys: &System) {
    if register_system(sys) {
        ctrlc::set_handler(move || handle_signal(Signal::Int))
            .expect("Error setting Ctrl-C handler");
    }
}

#[cfg(target_family = "windows")]
/// Unregister signal handler.
pub(crate) fn stop(sys: &System) {
    if unregister_system(sys) {
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
extern "C" fn sig_segv(_: i32) {
    eprintln!("Stack Overflow:\n{:?}", backtrace::Backtrace::new());
    handle_signal(Signal::Segv);
}
