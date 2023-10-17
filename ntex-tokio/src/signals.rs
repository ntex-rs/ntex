use std::{
    cell::RefCell, future::Future, mem, pin::Pin, rc::Rc, task::Context, task::Poll,
};

use tokio::sync::oneshot;
use tokio::task::spawn_local;

thread_local! {
    static SRUN: RefCell<bool> = RefCell::new(false);
    static SHANDLERS: Rc<RefCell<Vec<oneshot::Sender<Signal>>>> = Default::default();
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

/// Register signal handler.
///
/// Signals are handled by oneshots, you have to re-register
/// after each signal.
pub fn signal() -> Option<oneshot::Receiver<Signal>> {
    if !SRUN.with(|v| *v.borrow()) {
        spawn_local(Signals::new());
    }
    SHANDLERS.with(|handlers| {
        let (tx, rx) = oneshot::channel();
        handlers.borrow_mut().push(tx);
        Some(rx)
    })
}

struct Signals {
    #[cfg(not(unix))]
    signal: Pin<Box<dyn Future<Output = std::io::Result<()>>>>,
    #[cfg(unix)]
    signals: Vec<(
        Signal,
        tokio::signal::unix::Signal,
        tokio::signal::unix::SignalKind,
    )>,
}

impl Signals {
    fn new() -> Signals {
        SRUN.with(|h| *h.borrow_mut() = true);

        #[cfg(not(unix))]
        {
            Signals {
                signal: Box::pin(tokio::signal::ctrl_c()),
            }
        }

        #[cfg(unix)]
        {
            use tokio::signal::unix;

            let sig_map = [
                (unix::SignalKind::interrupt(), Signal::Int),
                (unix::SignalKind::hangup(), Signal::Hup),
                (unix::SignalKind::terminate(), Signal::Term),
                (unix::SignalKind::quit(), Signal::Quit),
            ];

            let mut signals = Vec::new();
            for (kind, sig) in sig_map.iter() {
                match unix::signal(*kind) {
                    Ok(stream) => signals.push((*sig, stream, *kind)),
                    Err(e) => log::error!(
                        "Cannot initialize stream handler for {:?} err: {}",
                        sig,
                        e
                    ),
                }
            }

            Signals { signals }
        }
    }
}

impl Drop for Signals {
    fn drop(&mut self) {
        SRUN.with(|h| *h.borrow_mut() = false);
    }
}

impl Future for Signals {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        #[cfg(not(unix))]
        {
            if self.signal.as_mut().poll(cx).is_ready() {
                let handlers = SHANDLERS.with(|h| mem::take(&mut *h.borrow_mut()));
                for sender in handlers {
                    let _ = sender.send(Signal::Int);
                }
            }
            Poll::Pending
        }
        #[cfg(unix)]
        {
            for (sig, stream, kind) in self.signals.iter_mut() {
                loop {
                    if Pin::new(&mut *stream).poll_recv(cx).is_ready() {
                        let handlers = SHANDLERS.with(|h| mem::take(&mut *h.borrow_mut()));
                        for sender in handlers {
                            let _ = sender.send(*sig);
                        }
                        match tokio::signal::unix::signal(*kind) {
                            Ok(s) => {
                                *stream = s;
                                continue;
                            }
                            Err(e) => log::error!(
                                "Cannot initialize stream handler for {:?} err: {}",
                                sig,
                                e
                            ),
                        }
                    }
                    break;
                }
            }
            Poll::Pending
        }
    }
}
