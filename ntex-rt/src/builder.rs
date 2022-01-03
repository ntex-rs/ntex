use std::{cell::RefCell, future::Future, io, rc::Rc};

use async_channel::{unbounded, Sender};
use async_oneshot as oneshot;

use crate::{arbiter::Arbiter, arbiter::SystemArbiter, arbiter::SystemCommand, System};

/// Builder struct for a ntex runtime.
///
/// Either use `Builder::build` to create a system and start actors.
/// Alternatively, use `Builder::run` to start the tokio runtime and
/// run a function in its context.
pub struct Builder {
    /// Name of the System. Defaults to "ntex" if unset.
    name: String,
    /// Whether the Arbiter will stop the whole System on uncaught panic. Defaults to false.
    stop_on_panic: bool,
}

impl Builder {
    pub(super) fn new() -> Self {
        Builder {
            name: "ntex".into(),
            stop_on_panic: false,
        }
    }

    /// Sets the name of the System.
    pub fn name<N: AsRef<str>>(mut self, name: N) -> Self {
        self.name = name.as_ref().into();
        self
    }

    /// Sets the option 'stop_on_panic' which controls whether the System is stopped when an
    /// uncaught panic is thrown from a worker thread.
    ///
    /// Defaults to false.
    pub fn stop_on_panic(mut self, stop_on_panic: bool) -> Self {
        self.stop_on_panic = stop_on_panic;
        self
    }

    /// Create new System.
    ///
    /// This method panics if it can not create tokio runtime
    pub fn finish(self) -> SystemRunner {
        let (stop_tx, stop) = oneshot::oneshot();
        let (sys_sender, sys_receiver) = unbounded();
        let stop_on_panic = self.stop_on_panic;

        // system arbiter
        let arb = SystemArbiter::new(stop_tx, sys_receiver);

        // init system arbiter and run configuration method
        SystemRunner {
            stop,
            arb,
            sys_sender,
            stop_on_panic,
        }
    }
}

/// Helper object that runs System's event loop
#[must_use = "SystemRunner must be run"]
pub struct SystemRunner {
    stop: oneshot::Receiver<i32>,
    arb: SystemArbiter,
    sys_sender: Sender<SystemCommand>,
    stop_on_panic: bool,
}

impl SystemRunner {
    /// This function will start event loop and will finish once the
    /// `System::stop()` function is called.
    pub fn run(self) -> io::Result<()> {
        self.run_with(|| ())
    }

    /// This function will start event loop and will finish once the
    /// `System::stop()` function is called.
    #[inline]
    pub fn run_with<F>(self, f: F) -> io::Result<()>
    where
        F: FnOnce() + 'static,
    {
        let SystemRunner {
            stop,
            arb,
            sys_sender,
            stop_on_panic,
        } = self;

        // run loop
        match block_on(stop, arb, sys_sender, stop_on_panic, f).take() {
            Ok(code) => {
                if code != 0 {
                    Err(io::Error::new(
                        io::ErrorKind::Other,
                        format!("Non-zero exit code: {}", code),
                    ))
                } else {
                    Ok(())
                }
            }
            Err(_) => Err(io::Error::new(io::ErrorKind::Other, "Closed")),
        }
    }
}

pub struct BlockResult<T>(Rc<RefCell<Option<T>>>);

impl<T> BlockResult<T> {
    pub fn take(self) -> T {
        self.0.borrow_mut().take().unwrap()
    }
}

#[inline]
fn block_on<F, F1, R>(
    fut: F,
    arb: SystemArbiter,
    sys_sender: Sender<SystemCommand>,
    stop_on_panic: bool,
    f: F1,
) -> BlockResult<R>
where
    F: Future<Output = R> + 'static,
    R: 'static,
    F1: FnOnce() + 'static,
{
    let result = Rc::new(RefCell::new(None));
    let result_inner = result.clone();
    crate::block_on(Box::pin(async move {
        let _system = System::construct(sys_sender, Arbiter::new_system(), stop_on_panic);
        crate::spawn(arb);
        f();
        let r = fut.await;
        *result_inner.borrow_mut() = Some(r);
    }));
    BlockResult(result)
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;

    use super::*;

    #[test]
    fn test_async() {
        let (tx, rx) = mpsc::channel();

        thread::spawn(move || {
            let runner = crate::System::build().stop_on_panic(true).finish();

            tx.send(System::current()).unwrap();
            let _ = runner.run();
        });
        let s = System::new("test");

        let sys = rx.recv().unwrap();
        let id = sys.id();
        let (tx, rx) = mpsc::channel();
        sys.arbiter().exec_fn(move || {
            let _ = tx.send(System::current().id());
        });
        let id2 = rx.recv().unwrap();
        assert_eq!(id, id2);

        let id2 = s
            .block_on(sys.arbiter().exec(|| System::current().id()))
            .unwrap();
        assert_eq!(id, id2);

        let (tx, rx) = mpsc::channel();
        sys.arbiter().spawn(Box::pin(async move {
            let _ = tx.send(System::current().id());
        }));
        let id2 = rx.recv().unwrap();
        assert_eq!(id, id2);
    }
}
