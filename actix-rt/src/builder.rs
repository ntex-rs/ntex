use std::borrow::Cow;
use std::io;

use futures::channel::mpsc::unbounded;
use futures::channel::oneshot::{channel, Receiver};
use futures::future::{lazy, Future};
use futures::{future, FutureExt};

use tokio::runtime::current_thread::Handle;
use tokio_net::driver::Reactor;
use tokio_timer::{clock::Clock, timer::Timer};

use crate::arbiter::{Arbiter, SystemArbiter};
use crate::runtime::Runtime;
use crate::system::System;
use tokio_executor::current_thread::CurrentThread;

/// Builder struct for a actix runtime.
///
/// Either use `Builder::build` to create a system and start actors.
/// Alternatively, use `Builder::run` to start the tokio runtime and
/// run a function in its context.
pub struct Builder {
    /// Name of the System. Defaults to "actix" if unset.
    name: Cow<'static, str>,

    /// The clock to use
    clock: Clock,

    /// Whether the Arbiter will stop the whole System on uncaught panic. Defaults to false.
    stop_on_panic: bool,
}

impl Builder {
    pub(crate) fn new() -> Self {
        Builder {
            name: Cow::Borrowed("actix"),
            clock: Clock::new(),
            stop_on_panic: false,
        }
    }

    /// Sets the name of the System.
    pub fn name<T: Into<String>>(mut self, name: T) -> Self {
        self.name = Cow::Owned(name.into());
        self
    }

    /// Set the Clock instance that will be used by this System.
    ///
    /// Defaults to the system clock.
    pub fn clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
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
    pub(crate) fn build_async(self, executor: Handle) -> AsyncSystemRunner {
        self.create_async_runtime(executor)
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

    fn create_async_runtime(self, executor: Handle) -> AsyncSystemRunner {
        let (stop_tx, stop) = channel();
        let (sys_sender, sys_receiver) = unbounded();

        let system = System::construct(sys_sender, Arbiter::new_system(), self.stop_on_panic);

        // system arbiter
        let arb = SystemArbiter::new(stop_tx, sys_receiver);

        // start the system arbiter
        executor.spawn(arb).expect("could not start system arbiter");

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

        let mut rt = self.build_rt().unwrap();
        rt.spawn(arb);

        // init system arbiter and run configuration method
        let _ = rt.block_on(lazy(move |_| {
            f();
            Ok::<_, ()>(())
        }));

        SystemRunner { rt, stop, system }
    }

    pub(crate) fn build_rt(&self) -> io::Result<Runtime> {
        // We need a reactor to receive events about IO objects from kernel
        let reactor = Reactor::new()?;
        let reactor_handle = reactor.handle();

        // Place a timer wheel on top of the reactor. If there are no timeouts to fire, it'll let the
        // reactor pick up some new external events.
        let timer = Timer::new_with_now(reactor, self.clock.clone());
        let timer_handle = timer.handle();

        // And now put a single-threaded executor on top of the timer. When there are no futures ready
        // to do something, it'll let the timer or the reactor to generate some new stimuli for the
        // futures to continue in their life.
        let executor = CurrentThread::new_with_park(timer);

        Ok(Runtime::new2(
            reactor_handle,
            timer_handle,
            self.clock.clone(),
            executor,
        ))
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
        future::lazy(|_| {
            Arbiter::run_system();
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
        let _ = rt.block_on(async {
            Arbiter::run_system();
            Ok::<_, ()>(())
        });
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
        F: Future<Output = O>,
    {
        let _ = self.rt.block_on(async {
            Arbiter::run_system();
        });

        let res = self.rt.block_on(fut);
        let _ = self.rt.block_on(async {
            Arbiter::stop_system();
        });

        res
    }
}
