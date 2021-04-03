use std::{future::Future, pin::Pin, task::Context, task::Poll};

use crate::server::Server;

/// Different types of process signals
#[allow(dead_code)]
#[derive(PartialEq, Clone, Copy, Debug)]
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

pub(super) struct Signals {
    srv: Server,
    #[cfg(not(unix))]
    signal: Pin<Box<dyn Future<Output = std::io::Result<()>>>>,
    #[cfg(unix)]
    signals: Vec<(Signal, crate::rt::signal::unix::Signal)>,
}

impl Signals {
    pub(super) fn new(srv: Server) -> Signals {
        #[cfg(not(unix))]
        {
            Signals {
                srv,
                signal: Box::pin(crate::rt::signal::ctrl_c()),
            }
        }

        #[cfg(unix)]
        {
            use crate::rt::signal::unix;

            let sig_map = [
                (unix::SignalKind::interrupt(), Signal::Int),
                (unix::SignalKind::hangup(), Signal::Hup),
                (unix::SignalKind::terminate(), Signal::Term),
                (unix::SignalKind::quit(), Signal::Quit),
            ];

            let mut signals = Vec::new();
            for (kind, sig) in sig_map.iter() {
                match unix::signal(*kind) {
                    Ok(stream) => signals.push((*sig, stream)),
                    Err(e) => log::error!(
                        "Cannot initialize stream handler for {:?} err: {}",
                        sig,
                        e
                    ),
                }
            }

            Signals { srv, signals }
        }
    }
}

impl Future for Signals {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        #[cfg(not(unix))]
        match self.signal.as_mut().poll(cx) {
            Poll::Ready(_) => {
                self.srv.signal(Signal::Int);
                Poll::Ready(())
            }
            Poll::Pending => Poll::Pending,
        }
        #[cfg(unix)]
        {
            let mut sigs = Vec::new();
            for (sig, fut) in self.signals.iter_mut() {
                if Pin::new(fut).poll_recv(cx).is_ready() {
                    sigs.push(*sig)
                }
            }
            for sig in sigs {
                self.srv.signal(sig);
            }
            Poll::Pending
        }
    }
}
