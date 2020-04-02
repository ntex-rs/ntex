use std::future::Future;
use std::io;
use tokio::{runtime, task::LocalSet};

/// Single-threaded runtime provides a way to start reactor
/// and runtime on the current thread.
///
/// See [module level][mod] documentation for more details.
///
/// [mod]: index.html
#[derive(Debug)]
pub struct Runtime {
    local: LocalSet,
    rt: runtime::Runtime,
}

impl Runtime {
    #[allow(clippy::new_ret_no_self)]
    /// Returns a new runtime initialized with default configuration values.
    pub fn new() -> io::Result<Runtime> {
        let rt = runtime::Builder::new()
            .enable_io()
            .enable_time()
            .basic_scheduler()
            .build()?;

        Ok(Runtime {
            rt,
            local: LocalSet::new(),
        })
    }

    pub(super) fn local(&self) -> &LocalSet {
        &self.local
    }

    /// Spawn a future onto the single-threaded runtime.
    ///
    /// See [module level][mod] documentation for more details.
    ///
    /// [mod]: index.html
    ///
    /// # Examples
    ///
    /// ```rust,ignore
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
    pub fn spawn<F>(&self, future: F) -> &Self
    where
        F: Future<Output = ()> + 'static,
    {
        self.local.spawn_local(future);
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
        self.local.block_on(&mut self.rt, f)
    }
}
