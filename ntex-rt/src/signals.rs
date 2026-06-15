use parking_lot::Mutex;
use std::cell::RefCell;

use crate::System;

thread_local! {
    static HANDLERS: RefCell<Vec<oneshot::Sender<Signal>>> = RefCell::default();
}

static CUR_SYS: Mutex<RefCell<Option<System>>> = Mutex::new(RefCell::new(None));

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
pub fn signal() -> oneshot::AsyncReceiver<Signal> {
    let (tx, rx) = oneshot::async_channel();
    System::current().handle().spawn(async move {
        HANDLERS.with(|handlers| {
            handlers.borrow_mut().push(tx);
        });
    });

    rx
}

fn register_system(sys: &System) -> bool {
    if sys.signals_disabled() {
        true
    } else {
        let guard = CUR_SYS.lock();

        let mut store = guard.borrow_mut();
        let started = store.is_some();
        *store = Some(sys.clone());
        started
    }
}

fn handle_signal(sig: Signal) {
    let guard = CUR_SYS.lock();
    if let Some(sys) = &*guard.borrow() {
        sys.handle().spawn(async move {
            HANDLERS.with(|handlers| {
                for tx in handlers.borrow_mut().drain(..) {
                    let _ = tx.send(sig);
                }
            });
        });
    }
}

#[cfg(target_family = "unix")]
/// Register signal handler.
///
/// Signals are handled by oneshots, you have to re-register
/// after each signal.
pub(crate) fn start(sys: &System) {
    if !register_system(sys) {
        use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGQUIT, SIGTERM, SIGUSR2};
        use signal_hook::low_level::register;

        for (s, sig) in [
            (SIGHUP, Signal::Hup),
            (SIGINT, Signal::Int),
            (SIGTERM, Signal::Term),
            (SIGQUIT, Signal::Quit),
        ] {
            let _ = unsafe { register(s, move || handle_signal(sig)) };
        }

        let _ = unsafe { register(SIGUSR2, || crate::system::sig_usr2()) };
    }
}

#[cfg(target_family = "windows")]
/// Register signal handler.
///
/// Signals are handled by oneshots, you have to re-register
/// after each signal.
pub(crate) fn start(sys: &System) {
    if !register_system(sys) {
        let _ = std::thread::Builder::new()
            .name("ntex signals".to_string())
            .spawn(move || {
                ctrlc::set_handler(move || handle_signal(Signal::Int))
                    .expect("Error setting Ctrl-C handler");
            });
    }
}
