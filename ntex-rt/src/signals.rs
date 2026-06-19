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
            if cur.id() != sys.id() {
                CUR_SYS = Some(cur);
                false
            } else {
                sys.handle().spawn(async move {
                    STOP.with(|stop| {
                        if let Some(tx) = stop.borrow_mut().take() {
                            let _ = tx.send(());
                        }
                    })
                });
                true
            }
        } else {
            false
        }
    }
}

fn handle_signal(sig: Signal) {
    unsafe {
        for idx in 0..10 {
            if SIGS[idx].is_none() {
                SIGS[idx] = Some(sig);
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
    if register_system(sys) {
        use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGQUIT, SIGTERM, SIGUSR2};
        use signal_hook::low_level::register;

        for (idx, s, sig) in [
            (0, SIGHUP, Signal::Hup),
            (1, SIGINT, Signal::Int),
            (2, SIGTERM, Signal::Term),
            (3, SIGQUIT, Signal::Quit),
        ] {
            let _ = unsafe {
                match register(s, move || handle_signal(sig)) {
                    Ok(s) => SIG_HANDLERS[idx] = Some(s),
                    Err(e) => {
                        log::error!("Cannot install signal handler for {sig:?} with {e:?}")
                    }
                }
            };
        }

        let _ = unsafe {
            match register(SIGUSR2, || crate::system::sig_usr2()) {
                Ok(s) => SIG_HANDLERS[5] = Some(s),
                Err(_) => log::error!("Cannot install signal handler for SIGUSR2"),
            }
        };
    }
}

#[cfg(target_family = "unix")]
/// Unregister signal handler.
pub(crate) fn stop(sys: &System) {
    if unregister_system(sys) {
        use signal_hook::low_level::unregister;

        for idx in 0..10 {
            unsafe {
                if let Some(s) = SIG_HANDLERS[idx].take() {
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
            for idx in 0..10 {
                if let Some(sig) = unsafe { SIGS[idx].take() } {
                    sigs.push(sig);
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
