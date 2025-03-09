#![allow(clippy::type_complexity)]
use std::any::{Any, TypeId};
use std::collections::{HashMap, VecDeque};
use std::{
    cell::Cell, cell::RefCell, future::Future, io, sync::Arc, task::Context, thread,
    time::Duration,
};

use async_task::{Runnable, Task};
use crossbeam_queue::SegQueue;
use ntex_iodriver::{
    op::Asyncify, AsRawFd, Key, NotifyHandle, OpCode, Proactor, ProactorBuilder, PushEntry,
    RawFd,
};

use crate::op::OpFuture;

scoped_tls::scoped_thread_local!(static CURRENT_RUNTIME: Runtime);

/// Type alias for `Task<Result<T, Box<dyn Any + Send>>>`, which resolves to an
/// `Err` when the spawned future panicked.
pub type JoinHandle<T> = Task<Result<T, Box<dyn Any + Send>>>;

pub struct RemoteHandle {
    handle: NotifyHandle,
    runnables: Arc<RunnableQueue>,
}

impl RemoteHandle {
    /// Wake up runtime
    pub fn notify(&self) {
        self.handle.notify().ok();
    }

    /// Spawns a new asynchronous task, returning a [`Task`] for it.
    ///
    /// Spawning a task enables the task to execute concurrently to other tasks.
    /// There is no guarantee that a spawned task will execute to completion.
    pub fn spawn<F: Future + Send + 'static>(&self, future: F) -> Task<F::Output> {
        let runnables = self.runnables.clone();
        let handle = self.handle.clone();
        let schedule = move |runnable| {
            runnables.schedule(runnable, &handle);
        };
        let (runnable, task) = unsafe { async_task::spawn_unchecked(future, schedule) };
        runnable.schedule();
        task
    }
}

/// The async runtime for ntex. It is a thread local runtime, and cannot be
/// sent to other threads.
pub struct Runtime {
    driver: Proactor,
    runnables: Arc<RunnableQueue>,
    event_interval: usize,
    data: RefCell<HashMap<TypeId, Box<dyn Any>, fxhash::FxBuildHasher>>,
}

impl Runtime {
    /// Create [`Runtime`] with default config.
    pub fn new() -> io::Result<Self> {
        Self::builder().build()
    }

    /// Create a builder for [`Runtime`].
    pub fn builder() -> RuntimeBuilder {
        RuntimeBuilder::new()
    }

    #[allow(clippy::arc_with_non_send_sync)]
    fn with_builder(builder: &RuntimeBuilder) -> io::Result<Self> {
        Ok(Self {
            driver: builder.proactor_builder.build()?,
            runnables: Arc::new(RunnableQueue::new()),
            event_interval: builder.event_interval,
            data: RefCell::new(HashMap::default()),
        })
    }

    /// Perform a function on the current runtime.
    ///
    /// ## Panics
    ///
    /// This method will panic if there are no running [`Runtime`].
    pub fn with_current<T, F: FnOnce(&Self) -> T>(f: F) -> T {
        #[cold]
        fn not_in_ntex_runtime() -> ! {
            panic!("not in a ntex runtime")
        }

        if CURRENT_RUNTIME.is_set() {
            CURRENT_RUNTIME.with(f)
        } else {
            not_in_ntex_runtime()
        }
    }

    /// Get current driver
    pub fn driver(&self) -> &Proactor {
        &self.driver
    }

    /// Get handle for current runtime
    pub fn handle(&self) -> RemoteHandle {
        RemoteHandle {
            handle: self.driver.handle(),
            runnables: self.runnables.clone(),
        }
    }

    /// Attach a raw file descriptor/handle/socket to the runtime.
    ///
    /// You only need this when authoring your own high-level APIs. High-level
    /// resources in this crate are attached automatically.
    pub fn attach(&self, fd: RawFd) -> io::Result<()> {
        self.driver.attach(fd)
    }

    /// Block on the future till it completes.
    pub fn block_on<F: Future>(&self, future: F) -> F::Output {
        CURRENT_RUNTIME.set(self, || {
            let mut result = None;
            unsafe { self.spawn_unchecked(async { result = Some(future.await) }) }.detach();

            self.runnables.run(self.event_interval);
            loop {
                if let Some(result) = result.take() {
                    return result;
                }

                self.poll_with_driver(self.runnables.has_tasks(), || {
                    self.runnables.run(self.event_interval);
                });
            }
        })
    }

    /// Spawns a new asynchronous task, returning a [`Task`] for it.
    ///
    /// Spawning a task enables the task to execute concurrently to other tasks.
    /// There is no guarantee that a spawned task will execute to completion.
    pub fn spawn<F: Future + 'static>(&self, future: F) -> Task<F::Output> {
        unsafe { self.spawn_unchecked(future) }
    }

    /// Spawns a new asynchronous task, returning a [`Task`] for it.
    ///
    /// # Safety
    ///
    /// The caller should ensure the captured lifetime is long enough.
    pub unsafe fn spawn_unchecked<F: Future>(&self, future: F) -> Task<F::Output> {
        let runnables = self.runnables.clone();
        let handle = self.driver.handle();
        let schedule = move |runnable| {
            runnables.schedule(runnable, &handle);
        };
        let (runnable, task) = async_task::spawn_unchecked(future, schedule);
        runnable.schedule();
        task
    }

    /// Spawns a blocking task in a new thread, and wait for it.
    ///
    /// The task will not be cancelled even if the future is dropped.
    pub fn spawn_blocking<F, R>(&self, f: F) -> JoinHandle<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        let op = Asyncify::new(move || {
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
            (Ok(0), res)
        });
        // It is safe to use `submit` here because the task is spawned immediately.
        unsafe {
            let fut = self.submit_with_flags(op);
            self.spawn_unchecked(async move { fut.await.0 .1.into_inner() })
        }
    }

    fn submit_raw<T: OpCode + 'static>(
        &self,
        op: T,
    ) -> PushEntry<Key<T>, (io::Result<usize>, T)> {
        self.driver.push(op)
    }

    fn submit_with_flags<T: OpCode + 'static>(
        &self,
        op: T,
    ) -> impl Future<Output = ((io::Result<usize>, T), u32)> {
        let fut = self.submit_raw(op);

        async move {
            match fut {
                PushEntry::Pending(user_data) => OpFuture::new(user_data).await,
                PushEntry::Ready(res) => {
                    // submit_flags won't be ready immediately, if ready, it must be error without
                    // flags
                    (res, 0)
                }
            }
        }
    }

    pub(crate) fn cancel_op<T: OpCode>(&self, op: Key<T>) {
        self.driver.cancel(op);
    }

    pub(crate) fn poll_task<T: OpCode>(
        &self,
        cx: &mut Context,
        op: Key<T>,
    ) -> PushEntry<Key<T>, ((io::Result<usize>, T), u32)> {
        self.driver.pop(op).map_pending(|mut k| {
            self.driver.update_waker(&mut k, cx.waker().clone());
            k
        })
    }

    fn poll_with_driver<F: FnOnce()>(&self, has_tasks: bool, f: F) {
        let timeout = if has_tasks {
            Some(Duration::ZERO)
        } else {
            None
        };

        if let Err(e) = self.driver.poll(timeout, f) {
            match e.kind() {
                io::ErrorKind::TimedOut | io::ErrorKind::Interrupted => {
                    log::debug!("expected error: {e}");
                }
                _ => panic!("{e:?}"),
            }
        }
    }

    /// Insert a type into this runtime.
    pub fn insert<T: 'static>(&self, val: T) {
        self.data
            .borrow_mut()
            .insert(TypeId::of::<T>(), Box::new(val));
    }

    /// Check if container contains entry
    pub fn contains<T: 'static>(&self) -> bool {
        self.data.borrow().contains_key(&TypeId::of::<T>())
    }

    /// Get a reference to a type previously inserted on this runtime.
    pub fn get<T>(&self) -> Option<T>
    where
        T: Clone + 'static,
    {
        self.data
            .borrow()
            .get(&TypeId::of::<T>())
            .and_then(|boxed| boxed.downcast_ref().cloned())
    }
}

impl AsRawFd for Runtime {
    fn as_raw_fd(&self) -> RawFd {
        self.driver.as_raw_fd()
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        CURRENT_RUNTIME.set(self, || {
            while self.runnables.sync_runnables.pop().is_some() {}
            loop {
                let runnable = self.runnables.local_runnables.borrow_mut().pop_front();
                if runnable.is_none() {
                    break;
                }
            }
        })
    }
}

struct RunnableQueue {
    id: thread::ThreadId,
    idle: Cell<bool>,
    local_runnables: RefCell<VecDeque<Runnable>>,
    sync_runnables: SegQueue<Runnable>,
}

impl RunnableQueue {
    fn new() -> Self {
        Self {
            id: thread::current().id(),
            idle: Cell::new(true),
            local_runnables: RefCell::new(VecDeque::new()),
            sync_runnables: SegQueue::new(),
        }
    }

    fn schedule(&self, runnable: Runnable, handle: &NotifyHandle) {
        if self.id == thread::current().id() {
            self.local_runnables.borrow_mut().push_back(runnable);
            if self.idle.get() {
                let _ = handle.notify();
            }
        } else {
            self.sync_runnables.push(runnable);
            handle.notify().ok();
        }
    }

    fn run(&self, event_interval: usize) {
        self.idle.set(false);
        for _ in 0..event_interval {
            let task = self.local_runnables.borrow_mut().pop_front();
            if let Some(task) = task {
                task.run();
            } else {
                break;
            }
        }

        for _ in 0..event_interval {
            if !self.sync_runnables.is_empty() {
                if let Some(task) = self.sync_runnables.pop() {
                    task.run();
                    continue;
                }
            }
            break;
        }
        self.idle.set(true);
    }

    fn has_tasks(&self) -> bool {
        !(self.local_runnables.borrow().is_empty() && self.sync_runnables.is_empty())
    }
}

/// Builder for [`Runtime`].
#[derive(Debug, Clone)]
pub struct RuntimeBuilder {
    proactor_builder: ProactorBuilder,
    event_interval: usize,
}

impl Default for RuntimeBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl RuntimeBuilder {
    /// Create the builder with default config.
    pub fn new() -> Self {
        Self {
            proactor_builder: ProactorBuilder::new(),
            event_interval: 61,
        }
    }

    /// Replace proactor builder.
    pub fn with_proactor(&mut self, builder: ProactorBuilder) -> &mut Self {
        self.proactor_builder = builder;
        self
    }

    /// Sets the number of scheduler ticks after which the scheduler will poll
    /// for external events (timers, I/O, and so on).
    ///
    /// A scheduler “tick” roughly corresponds to one poll invocation on a task.
    pub fn event_interval(&mut self, val: usize) -> &mut Self {
        self.event_interval = val;
        self
    }

    /// Build [`Runtime`].
    pub fn build(&self) -> io::Result<Runtime> {
        Runtime::with_builder(self)
    }
}

/// Spawns a new asynchronous task, returning a [`Task`] for it.
///
/// Spawning a task enables the task to execute concurrently to other tasks.
/// There is no guarantee that a spawned task will execute to completion.
///
/// ## Panics
///
/// This method doesn't create runtime. It tries to obtain the current runtime
/// by [`Runtime::with_current`].
pub fn spawn<F: Future + 'static>(future: F) -> Task<F::Output> {
    Runtime::with_current(|r| r.spawn(future))
}

/// Spawns a blocking task in a new thread, and wait for it.
///
/// The task will not be cancelled even if the future is dropped.
///
/// ## Panics
///
/// This method doesn't create runtime. It tries to obtain the current runtime
/// by [`Runtime::with_current`].
pub fn spawn_blocking<T: Send + 'static>(
    f: impl (FnOnce() -> T) + Send + 'static,
) -> JoinHandle<T> {
    Runtime::with_current(|r| r.spawn_blocking(f))
}

/// Submit an operation to the current runtime, and return a future for it.
///
/// ## Panics
///
/// This method doesn't create runtime. It tries to obtain the current runtime
/// by [`Runtime::with_current`].
pub async fn submit<T: OpCode + 'static>(op: T) -> (io::Result<usize>, T) {
    submit_with_flags(op).await.0
}

/// Submit an operation to the current runtime, and return a future for it with
/// flags.
///
/// ## Panics
///
/// This method doesn't create runtime. It tries to obtain the current runtime
/// by [`Runtime::with_current`].
pub async fn submit_with_flags<T: OpCode + 'static>(
    op: T,
) -> ((io::Result<usize>, T), u32) {
    Runtime::with_current(|r| r.submit_with_flags(op)).await
}
