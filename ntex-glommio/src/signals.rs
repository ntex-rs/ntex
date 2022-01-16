use std::{cell::RefCell, future::Future, pin::Pin, rc::Rc, task::Context, task::Poll};

use async_oneshot as oneshot;
use glommio::Task;

thread_local! {
    static SRUN: RefCell<bool> = RefCell::new(false);
    static SHANDLERS: Rc<RefCell<Vec<oneshot::Sender<Signal>>>> = Default::default();
}

/// Different types of process signals
#[derive(PartialEq, Clone, Copy, Debug)]
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
        Task::local(Signals::new()).detach();
    }
    SHANDLERS.with(|handlers| {
        let (tx, rx) = oneshot::oneshot();
        handlers.borrow_mut().push(tx);
        Some(rx)
    })
}

struct Signals {}

impl Signals {
    pub(super) fn new() -> Signals {
        Self {}
    }
}

impl Future for Signals {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        Poll::Ready(())
    }
}
