use std::{borrow::Cow, future::Future, io};

use ntex_util::future::lazy;
use tokio::sync::mpsc::unbounded_channel;
use tokio::sync::oneshot::{channel, Receiver};
use tokio::task::LocalSet;

use super::arbiter::{Arbiter, SystemArbiter};
use super::runtime::Runtime;
use super::system::System;

/// Builder struct for a ntex runtime.
///
/// Either use `Builder::build` to create a system and start actors.
/// Alternatively, use `Builder::run` to start the tokio runtime and
/// run a function in its context.
pub struct Builder {
    /// Name of the System. Defaults to "ntex" if unset.
    name: Cow<'static, str>,

    /// Whether the Arbiter will stop the whole System on uncaught panic. Defaults to false.
    stop_on_panic: bool,
}

impl Builder {
    pub(super) fn new() -> Self {
        Builder {
            name: Cow::Borrowed("ntex"),
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
    pub fn finish(self) -> SystemRunner {
        self.create_runtime(|| {})
    }

    /// Create new System that can run asynchronously.
    /// This method could be used to run ntex system in existing tokio
    /// runtime.
    ///
    /// This method panics if it cannot start the system arbiter
    pub fn finish_with(self, local: &LocalSet) -> AsyncSystemRunner {
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
        let (sys_sender, sys_receiver) = unbounded_channel();

        let system = System::construct(
            sys_sender,
            Arbiter::new_system(local),
            self.stop_on_panic,
        );

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
        let (sys_sender, sys_receiver) = unbounded_channel();

        let rt = Runtime::new().unwrap();

        // system arbiter
        let system = System::construct(
            sys_sender,
            Arbiter::new_system(rt.local()),
            self.stop_on_panic,
        );
        let arb = SystemArbiter::new(stop_tx, sys_receiver);
        rt.spawn(arb);

        // init system arbiter and run configuration method
        rt.block_on(lazy(move |_| f()));

        SystemRunner { rt, stop, system }
    }
}

#[derive(Debug)]
pub struct AsyncSystemRunner {
    stop: Receiver<i32>,
    system: System,
}

impl AsyncSystemRunner {
    /// This function will start event loop and returns a future that
    /// resolves once the `System::stop()` function is called.
    pub fn run(self) -> impl Future<Output = Result<(), io::Error>> + Send {
        let AsyncSystemRunner { stop, .. } = self;

        // run loop
        async move {
            match stop.await {
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
            }
        }
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
        let SystemRunner { rt, stop, .. } = self;

        // run loop
        match rt.block_on(stop) {
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
        }
    }

    /// Execute a future and wait for result.
    #[inline]
    pub fn block_on<F, O>(&mut self, fut: F) -> O
    where
        F: Future<Output = O>,
    {
        self.rt.block_on(fut)
    }

    /// Execute a function with enabled executor.
    #[inline]
    pub fn exec<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        self.rt.block_on(lazy(|_| f()))
    }
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
            let rt = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            let local = tokio::task::LocalSet::new();

            let runner = crate::System::build()
                .stop_on_panic(true)
                .finish_with(&local);

            tx.send(System::current()).unwrap();
            let _ = rt.block_on(local.run_until(runner.run()));
        });
        let mut s = System::new("test");

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
