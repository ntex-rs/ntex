use std::{cell::RefCell, thread};

use ntex_rt::System;

use crate::server::Server;

thread_local! {
    static HANDLERS: RefCell<Vec<oneshot::Sender<Signal>>> = Default::default();
}

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

#[doc(hidden)]
/// Register signal handler.
pub fn signal() -> oneshot::Receiver<Signal> {
    let (tx, rx) = oneshot::channel();
    System::current().arbiter().exec_fn(|| {
        HANDLERS.with(|handlers| {
            handlers.borrow_mut().push(tx);
        })
    });

    rx
}

#[cfg(target_family = "unix")]
/// Register signal handler.
///
/// Signals are handled by oneshots, you have to re-register
/// after each signal.
pub(crate) fn start<T: Send + 'static>(srv: Server<T>) {
    let _ = thread::Builder::new()
        .name("ntex-server signals".to_string())
        .spawn(move || {
            use signal_hook::consts::signal::*;
            use signal_hook::iterator::Signals;

            let sigs = vec![SIGHUP, SIGINT, SIGTERM, SIGQUIT];
            let mut signals = match Signals::new(sigs) {
                Ok(signals) => signals,
                Err(e) => {
                    log::error!("Cannot initialize signals handler: {}", e);
                    return;
                }
            };
            for info in &mut signals {
                let sig = match info {
                    SIGHUP => Signal::Hup,
                    SIGTERM => Signal::Term,
                    SIGINT => Signal::Int,
                    SIGQUIT => Signal::Quit,
                    _ => continue,
                };

                srv.signal(sig);
                System::current().arbiter().exec_fn(move || {
                    HANDLERS.with(|handlers| {
                        for tx in handlers.borrow_mut().drain(..) {
                            let _ = tx.send(sig);
                        }
                    })
                });

                if matches!(sig, Signal::Int | Signal::Quit) {
                    return;
                }
            }
        });
}

#[cfg(target_family = "windows")]
/// Register signal handler.
///
/// Signals are handled by oneshots, you have to re-register
/// after each signal.
pub(crate) fn start<T: Send + 'static>(srv: Server<T>) {
    let _ = thread::Builder::new()
        .name("ntex-server signals".to_string())
        .spawn(move || {
            ctrlc::set_handler(move || {
                srv.signal(Signal::Int);

                System::current().arbiter().exec_fn(|| {
                    HANDLERS.with(|handlers| {
                        for tx in handlers.borrow_mut().drain(..) {
                            let _ = tx.send(Signal::Int);
                        }
                    })
                });
            })
            .expect("Error setting Ctrl-C handler");
        });
}
