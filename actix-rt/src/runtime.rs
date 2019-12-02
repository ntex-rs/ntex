use std::error::Error;
use std::{fmt, io};

use futures::Future;
use tokio_executor::current_thread::{self, CurrentThread};
use tokio_net::driver::{Handle as ReactorHandle, Reactor};
use tokio_timer::{
    clock::Clock,
    timer::{self, Timer},
};

use crate::builder::Builder;

/// Single-threaded runtime provides a way to start reactor
/// and executor on the current thread.
///
/// See [module level][mod] documentation for more details.
///
/// [mod]: index.html
#[derive(Debug)]
pub struct Runtime {
    reactor_handle: ReactorHandle,
    timer_handle: timer::Handle,
    clock: Clock,
    executor: CurrentThread<Timer<Reactor>>,
}

/// Error returned by the `run` function.
#[derive(Debug)]
pub struct RunError {
    inner: current_thread::RunError,
}

impl fmt::Display for RunError {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(fmt, "{}", self.inner)
    }
}

impl Error for RunError {
    fn description(&self) -> &str {
        self.inner.description()
    }
    fn cause(&self) -> Option<&dyn Error> {
        self.inner.source()
    }
}

impl Runtime {
    #[allow(clippy::new_ret_no_self)]
    /// Returns a new runtime initialized with default configuration values.
    pub fn new() -> io::Result<Runtime> {
        Builder::new().build_rt()
    }

    pub(super) fn new2(
        reactor_handle: ReactorHandle,
        timer_handle: timer::Handle,
        clock: Clock,
        executor: CurrentThread<Timer<Reactor>>,
    ) -> Runtime {
        Runtime {
            reactor_handle,
            timer_handle,
            clock,
            executor,
        }
    }

    /// Spawn a future onto the single-threaded Tokio runtime.
    ///
    /// See [module level][mod] documentation for more details.
    ///
    /// [mod]: index.html
    ///
    /// # Examples
    ///
    /// ```rust
    /// # use futures::{future, Future, Stream};
    /// use actix_rt::Runtime;
    ///
    /// # fn dox() {
    /// // Create the runtime
    /// let mut rt = Runtime::new().unwrap();
    ///
    /// // Spawn a future onto the runtime
    /// rt.spawn(future::lazy(|_| {
    ///     println!("running on the runtime");
    /// }));
    /// # }
    /// # pub fn main() {}
    /// ```
    ///
    /// # Panics
    ///
    /// This function panics if the spawn fails. Failure occurs if the executor
    /// is currently at capacity and is unable to spawn a new future.
    pub fn spawn<F>(&mut self, future: F) -> &mut Self
    where
        F: Future<Output = ()> + 'static,
    {
        self.executor.spawn(future);
        self
    }

    /// Runs the provided future, blocking the current thread until the future
    /// completes.
    ///
    /// This function can be used to synchronously block the current thread
    /// until the provided `future` has resolved either successfully or with an
    /// error. The result of the future is then returned from this function
    /// call.
    ///
    /// Note that this function will **also** execute any spawned futures on the
    /// current thread, but will **not** block until these other spawned futures
    /// have completed. Once the function returns, any uncompleted futures
    /// remain pending in the `Runtime` instance. These futures will not run
    /// until `block_on` or `run` is called again.
    ///
    /// The caller is responsible for ensuring that other spawned futures
    /// complete execution by calling `block_on` or `run`.
    pub fn block_on<F>(&mut self, f: F) -> F::Output
    where
        F: Future,
    {
        self.enter(|executor| {
            // Run the provided future
            executor.block_on(f)
        })
    }

    /// Run the executor to completion, blocking the thread until **all**
    /// spawned futures have completed.
    pub fn run(&mut self) -> Result<(), RunError> {
        self.enter(|executor| executor.run())
            .map_err(|e| RunError { inner: e })
    }

    fn enter<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(&mut CurrentThread<Timer<Reactor>>) -> R,
    {
        let Runtime {
            ref reactor_handle,
            ref timer_handle,
            ref clock,
            ref mut executor,
            ..
        } = *self;

        // WARN: We do not enter the executor here, since in tokio 0.2 the executor is entered
        // automatically inside its `block_on` and `run` methods
        tokio_executor::with_default(&mut current_thread::TaskExecutor::current(), || {
            tokio_timer::clock::with_default(clock, || {
                let _reactor_guard = tokio_net::driver::set_default(reactor_handle);
                let _timer_guard = tokio_timer::set_default(timer_handle);
                f(executor)
            })
        })
    }
}
