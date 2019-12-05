use std::borrow::Cow;
use std::io;

use futures::channel::mpsc::unbounded;
use futures::channel::oneshot::{channel, Receiver};
use futures::future::{lazy, Future, FutureExt};
use tokio::task::LocalSet;

use crate::arbiter::{Arbiter, SystemArbiter};
use crate::runtime::Runtime;
use crate::system::System;

/// Builder struct for a actix runtime.
///
/// Either use `Builder::build` to create a system and start actors.
/// Alternatively, use `Builder::run` to start the tokio runtime and
/// run a function in its context.
pub struct Builder {
    /// Name of the System. Defaults to "actix" if unset.
    name: Cow<'static, str>,

    /// Whether the Arbiter will stop the whole System on uncaught panic. Defaults to false.
    stop_on_panic: bool,
}

impl Builder {
    pub(crate) fn new() -> Self {
        Builder {
            name: Cow::Borrowed("actix"),
            stop_on_panic: false,
        }
    }

    /// Sets the name of the System.
    pub fn name<T: Into<String>>(mut self, name: T) -> Self {
        self.name = Cow::Owned(name.into());
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
    pub fn build(self) -> SystemRunner {
        self.create_runtime(|| {})
    }

    /// Create new System that can run asynchronously.
    ///
    /// This method panics if it cannot start the system arbiter
    pub(crate) fn build_async(self, local: &LocalSet) -> AsyncSystemRunner {
        self.create_async_runtime(local)
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

    fn create_async_runtime(self, local: &LocalSet) -> AsyncSystemRunner {
        let (stop_tx, stop) = channel();
        let (sys_sender, sys_receiver) = unbounded();

        let system = System::construct(sys_sender, Arbiter::new_system(), self.stop_on_panic);

        // system arbiter
        let arb = SystemArbiter::new(stop_tx, sys_receiver);

        // start the system arbiter
        let _ = local.spawn_local(arb);

        AsyncSystemRunner { stop, system }
    }

    fn create_runtime<F>(self, f: F) -> SystemRunner
    where
        F: FnOnce() + 'static,
    {
        let (stop_tx, stop) = channel();
        let (sys_sender, sys_receiver) = unbounded();

        let system = System::construct(sys_sender, Arbiter::new_system(), self.stop_on_panic);

        // system arbiter
        let arb = SystemArbiter::new(stop_tx, sys_receiver);

        let mut rt = Runtime::new().unwrap();
        rt.spawn(arb);

        // init system arbiter and run configuration method
        rt.block_on(lazy(move |_| f()));

        SystemRunner { rt, stop, system }
    }
}

#[derive(Debug)]
pub(crate) struct AsyncSystemRunner {
    stop: Receiver<i32>,
    system: System,
}

impl AsyncSystemRunner {
    /// This function will start event loop and returns a future that
    /// resolves once the `System::stop()` function is called.
    pub(crate) fn run_nonblocking(self) -> impl Future<Output = Result<(), io::Error>> + Send {
        let AsyncSystemRunner { stop, .. } = self;

        // run loop
        lazy(|_| {
            Arbiter::run_system(None);
            async {
                let res = match stop.await {
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
                    Err(e) => Err(io::Error::new(io::ErrorKind::Other, e)),
                };
                Arbiter::stop_system();
                return res;
            }
        })
        .flatten()
    }
}

/// Helper object that runs System's event loop
#[must_use = "SystemRunner must be run"]
#[derive(Debug)]
pub struct SystemRunner {
    rt: Runtime,
    stop: Receiver<i32>,
    system: System,
}

impl SystemRunner {
    /// This function will start event loop and will finish once the
    /// `System::stop()` function is called.
    pub fn run(self) -> io::Result<()> {
        let SystemRunner { mut rt, stop, .. } = self;

        // run loop
        Arbiter::run_system(Some(&rt));
        let result = match rt.block_on(stop) {
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
            Err(e) => Err(io::Error::new(io::ErrorKind::Other, e)),
        };
        Arbiter::stop_system();
        result
    }

    /// Execute a future and wait for result.
    pub fn block_on<F, O>(&mut self, fut: F) -> O
    where
        F: Future<Output = O> + 'static,
    {
        Arbiter::run_system(Some(&self.rt));
        let res = self.rt.block_on(fut);
        Arbiter::stop_system();
        res
    }
}
