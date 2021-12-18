use std::{cell::RefCell, future::Future, io, rc::Rc};

use async_channel::unbounded;
use async_oneshot as oneshot;
use ntex_util::future::lazy;

use crate::arbiter::{Arbiter, SystemArbiter};
use crate::{create_runtime, Runtime, System};

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
        self.create_runtime(|| {})
    }

    /// This function will start tokio runtime and will finish once the
    /// `System::stop()` message get called.
    /// Function `f` get called within tokio runtime context.
    pub fn run<F>(self, f: F) -> io::Result<()>
    where
        F: FnOnce() + 'static,
    {
        self.create_runtime(f).run()
    }

    fn create_runtime<F>(self, f: F) -> SystemRunner
    where
        F: FnOnce() + 'static,
    {
        let (stop_tx, stop) = oneshot::oneshot();
        let (sys_sender, sys_receiver) = unbounded();

        let rt = create_runtime();

        // system arbiter
        let _system =
            System::construct(sys_sender, Arbiter::new_system(&rt), self.stop_on_panic);
        let arb = SystemArbiter::new(stop_tx, sys_receiver);
        rt.spawn(Box::pin(arb));

        // init system arbiter and run configuration method
        let runner = SystemRunner { rt, stop, _system };
        runner.block_on(lazy(move |_| f()));

        runner
    }
}

/// Helper object that runs System's event loop
#[must_use = "SystemRunner must be run"]
pub struct SystemRunner {
    rt: Box<dyn Runtime>,
    stop: oneshot::Receiver<i32>,
    _system: System,
}

impl SystemRunner {
    /// This function will start event loop and will finish once the
    /// `System::stop()` function is called.
    pub fn run(self) -> io::Result<()> {
        let SystemRunner { rt, stop, .. } = self;

        // run loop
        match block_on(&rt, stop).take() {
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

    /// Execute a future and wait for result.
    #[inline]
    pub fn block_on<F, R>(&self, fut: F) -> R
    where
        F: Future<Output = R> + 'static,
        R: 'static,
    {
        block_on(&self.rt, fut).take()
    }

    /// Execute a function with enabled executor.
    #[inline]
    pub fn exec<F, R>(&self, f: F) -> R
    where
        F: FnOnce() -> R + 'static,
        R: 'static,
    {
        self.block_on(lazy(|_| f()))
    }
}

pub struct BlockResult<T>(Rc<RefCell<Option<T>>>);

impl<T> BlockResult<T> {
    pub fn take(self) -> T {
        self.0.borrow_mut().take().unwrap()
    }
}

#[inline]
#[allow(clippy::borrowed_box)]
fn block_on<F, R>(rt: &Box<dyn Runtime>, fut: F) -> BlockResult<R>
where
    F: Future<Output = R> + 'static,
    R: 'static,
{
    let result = Rc::new(RefCell::new(None));
    let result_inner = result.clone();
    rt.block_on(Box::pin(async move {
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
