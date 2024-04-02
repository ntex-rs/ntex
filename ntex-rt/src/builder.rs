#![allow(clippy::let_underscore_future)]

use std::{cell::RefCell, future::Future, io, rc::Rc};

use async_channel::unbounded;

use crate::arbiter::{Arbiter, ArbiterController, SystemArbiter};
use crate::System;

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
    /// Use current Handler
    use_current_handle: bool,
}

impl Builder {
    #[cfg(feature = "tokio")]
    pub(super) fn new_with_handle() -> Self {
        Builder {
            name: "ntex".into(),
            stop_on_panic: false,
            use_current_handle: true,
        }
    }

    pub(super) fn new() -> Self {
        Builder {
            name: "ntex".into(),
            stop_on_panic: false,
            use_current_handle: false,
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
        let (stop_tx, stop) = oneshot::channel();
        let (sys_sender, sys_receiver) = unbounded();
        let stop_on_panic = self.stop_on_panic;

        let (arb, arb_controller) = Arbiter::new_system();
        let system = System::construct(sys_sender, arb, stop_on_panic);

        // system arbiter
        let arb = SystemArbiter::new(stop_tx, sys_receiver);

        // init system arbiter and run configuration method
        #[cfg(feature = "tokio")]
        {
            use tok_io::runtime::Handle;
            if !self.use_current_handle {
                log::info!("Using current handle is disabled!");
                return SystemRunner {
                    stop,
                    arb,
                    arb_controller,
                    system,
                    handle: None,
                };
            }

            let try_handle = match Handle::try_current() {
                Ok(handle) => handle,
                Err(_) => {
                    log::error!("No tokio runtime available");
                    return SystemRunner {
                        stop,
                        arb,
                        arb_controller,
                        system,
                        handle: None,
                    };
                }
            };

            SystemRunner {
                stop,
                arb,
                arb_controller,
                system,
                handle: Some(try_handle),
            }
        }

        #[cfg(not(feature = "tokio"))]
        {
            SystemRunner {
                stop,
                arb,
                arb_controller,
                system,
            }
        }
    }
}

#[cfg(not(feature = "tokio"))]
/// Helper object that runs System's event loop
#[must_use = "SystemRunner must be run"]
pub struct SystemRunner {
    stop: oneshot::Receiver<i32>,
    arb: SystemArbiter,
    arb_controller: ArbiterController,
    system: System,
}

#[cfg(feature = "tokio")]
use tok_io::runtime::Handle;

#[cfg(feature = "tokio")]
pub struct SystemRunner {
    stop: oneshot::Receiver<i32>,
    arb: SystemArbiter,
    arb_controller: ArbiterController,
    system: System,
    handle: Option<Handle>,
}

impl SystemRunner {
    /// Get current system.
    pub fn system(&self) -> System {
        self.system.clone()
    }

    /// This function will start event loop and will finish once the
    /// `System::stop()` function is called.
    pub fn run_until_stop(self) -> io::Result<()> {
        self.run(|| Ok(()))
    }

    /// This function will start event loop and will finish once the
    /// `System::stop()` function is called.
    #[inline]
    pub fn run<F>(self, f: F) -> io::Result<()>
    where
        F: FnOnce() -> io::Result<()> + 'static,
    {
        #[cfg(feature = "tokio")]
        {
            let SystemRunner {
                arb,
                arb_controller,
                stop,
                handle,
                ..
            } = self;

            if let Some(handle) = handle {
                let result = handle.block_on(async { f() });
                return result;
            }

            match crate::builder::block_on(stop, arb, arb_controller, f).take()? {
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

        // run loop
        #[cfg(not(feature = "tokio"))]
        {
            let SystemRunner {
                arb,
                arb_controller,
                stop,
                ..
            } = self;
            match crate::builder::block_on(stop, arb, arb_controller, f).take()? {
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

    /// Execute a future and wait for result.
    #[inline]
    pub fn block_on<F, R>(self, fut: F) -> R
    where
        F: Future<Output = R> + 'static,
        R: 'static,
    {
        #[cfg(feature = "tokio")]
        {
            let SystemRunner {
                handle,
                arb,
                arb_controller,
                ..
            } = self;

            if let Some(handle) = handle {
                let result = handle.block_on(fut);
                return result;
            }

            match crate::builder::block_on(fut, arb, arb_controller, || Ok(())).take() {
                Ok(result) => result,
                Err(_) => unreachable!(),
            }
        }

        // run loop
        #[cfg(not(feature = "tokio"))]
        {
            let SystemRunner {
                arb,
                arb_controller,
                ..
            } = self;
            match crate::builder::block_on(fut, arb, arb_controller, || Ok(())).take() {
                Ok(result) => result,
                Err(_) => unreachable!(),
            }
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
fn block_on<F, R, F1>(
    fut: F,
    arb: SystemArbiter,
    arb_controller: ArbiterController,
    f: F1,
) -> BlockResult<io::Result<R>>
where
    F: Future<Output = R> + 'static,
    R: 'static,
    F1: FnOnce() -> io::Result<()> + 'static,
{
    let result = Rc::new(RefCell::new(None));
    let result_inner = result.clone();
    crate::block_on(Box::pin(async move {
        let _ = crate::spawn(arb);
        let _ = crate::spawn(arb_controller);
        if let Err(e) = f() {
            *result_inner.borrow_mut() = Some(Err(e));
        } else {
            let r = fut.await;
            *result_inner.borrow_mut() = Some(Ok(r));
        }
    }));
    BlockResult(result)
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;

    use super::*;

    #[cfg(not(feature = "tokio"))]
    #[test]
    fn test_async() {
        let (tx, rx) = mpsc::channel();

        thread::spawn(move || {
            let runner = crate::System::build().stop_on_panic(true).finish();

            tx.send(runner.system()).unwrap();
            let _ = runner.run_until_stop();
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

    #[cfg(feature = "tokio")]
    #[tok_io::test]
    async fn test_async_tokio() {
        env_logger::init();

        let (tx, rx) = mpsc::channel();

        thread::spawn(move || {
            let runner = crate::System::build_with_handle()
                .stop_on_panic(true)
                .finish();

            tx.send(runner.system()).unwrap();
            let _ = runner.run_until_stop();
        });

        let s = System::new_with_handle("test");

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
