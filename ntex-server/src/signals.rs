use std::thread;

use crate::server::Server;

/// Different types of process signals
#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub(crate) enum Signal {
    /// SIGHUP
    Hup,
    /// SIGINT
    Int,
    /// SIGTERM
    Term,
    /// SIGQUIT
    Quit,
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
                match info {
                    SIGHUP => srv.signal(Signal::Hup),
                    SIGTERM => srv.signal(Signal::Term),
                    SIGINT => {
                        srv.signal(Signal::Int);
                        return;
                    }
                    SIGQUIT => {
                        srv.signal(Signal::Quit);
                        return;
                    }
                    _ => {}
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
                srv.send(Signal::Int);
            })
            .expect("Error setting Ctrl-C handler");
        });
}
