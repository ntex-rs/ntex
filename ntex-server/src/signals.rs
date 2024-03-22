use std::thread;

use signal_hook::consts::signal::*;
use signal_hook::iterator::exfiltrator::WithOrigin;
use signal_hook::iterator::SignalsInfo;

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

/// Register signal handler.
///
/// Signals are handled by oneshots, you have to re-register
/// after each signal.
pub(crate) fn start<T: Send + 'static>(srv: Server<T>) {
    let _ = thread::Builder::new()
        .name("ntex-server signals".to_string())
        .spawn(move || {
            let sigs = vec![SIGHUP, SIGINT, SIGTERM, SIGQUIT];

            let mut signals = match SignalsInfo::<WithOrigin>::new(&sigs) {
                Ok(signals) => signals,
                Err(e) => {
                    log::error!("Cannot initialize signals handler: {}", e);
                    return;
                }
            };
            for info in &mut signals {
                match info.signal {
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
