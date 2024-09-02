use std::{future::Future, io, pin::Pin, sync::Arc};

use async_channel::unbounded;

use crate::arbiter::{Arbiter, ArbiterController, SystemArbiter};
use crate::{system::SystemConfig, System};

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
    /// New thread stack size
    stack_size: usize,
    /// Block on fn
    block_on: Option<Arc<dyn Fn(Pin<Box<dyn Future<Output = ()>>>) + Sync + Send>>,
}

impl Builder {
    pub(super) fn new() -> Self {
        Builder {
            name: "ntex".into(),
            stop_on_panic: false,
            stack_size: 0,
            block_on: None,
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

    /// Sets the size of the stack (in bytes) for the new thread.
    pub fn stack_size(mut self, size: usize) -> Self {
        self.stack_size = size;
        self
    }

    /// Use custom block_on function
    pub fn block_on<F>(mut self, block_on: F) -> Self
    where
        F: Fn(Pin<Box<dyn Future<Output = ()>>>) + Sync + Send + 'static,
    {
        self.block_on = Some(Arc::new(block_on));
        self
    }

    /// Create new System.
    ///
    /// This method panics if it can not create tokio runtime
    pub fn finish(self) -> SystemRunner {
        let (stop_tx, stop) = oneshot::channel();
        let (sys_sender, sys_receiver) = unbounded();

        let config = SystemConfig {
            block_on: self.block_on,
            stack_size: self.stack_size,
            stop_on_panic: self.stop_on_panic,
        };

        let (arb, arb_controller) = Arbiter::new_system();
        let system = System::construct(sys_sender, arb, config);

        // system arbiter
        let arb = SystemArbiter::new(stop_tx, sys_receiver);

        // init system arbiter and run configuration method
        SystemRunner {
            stop,
            arb,
            arb_controller,
            system,
        }
    }
}

/// Helper object that runs System's event loop
#[must_use = "SystemRunner must be run"]
pub struct SystemRunner {
    stop: oneshot::Receiver<i32>,
    arb: SystemArbiter,
    arb_controller: ArbiterController,
    system: System,
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
        let SystemRunner {
            stop,
            arb,
            arb_controller,
            system,
            ..
        } = self;

        // run loop
        system.config().block_on(async move {
            f()?;

            let _ = crate::spawn(arb);
            let _ = crate::spawn(arb_controller);
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
                Err(_) => Err(io::Error::new(io::ErrorKind::Other, "Closed")),
            }
        })
    }

    /// Execute a future and wait for result.
    #[inline]
    pub fn block_on<F, R>(self, fut: F) -> R
    where
        F: Future<Output = R> + 'static,
        R: 'static,
    {
        let SystemRunner {
            arb,
            arb_controller,
            system,
            ..
        } = self;

        system.config().block_on(async move {
            let _ = crate::spawn(arb);
            let _ = crate::spawn(arb_controller);
            fut.await
        })
    }

    #[cfg(feature = "tokio")]
    /// Execute a future and wait for result.
    pub async fn run_local<F, R>(self, fut: F) -> R
    where
        F: Future<Output = R> + 'static,
        R: 'static,
    {
        let SystemRunner {
            arb,
            arb_controller,
            ..
        } = self;

        // run loop
        tok_io::task::LocalSet::new()
            .run_until(async move {
                let _ = crate::spawn(arb);
                let _ = crate::spawn(arb_controller);
                fut.await
            })
            .await
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
    #[test]
    fn test_block_on() {
        let (tx, rx) = mpsc::channel();

        thread::spawn(move || {
            let runner = crate::System::build()
                .stop_on_panic(true)
                .block_on(|fut| {
                    let rt = tok_io::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    tok_io::task::LocalSet::new().block_on(&rt, fut);
                })
                .finish();

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

        sys.stop();
    }
}
