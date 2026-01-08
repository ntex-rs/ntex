use std::cell::{Cell, UnsafeCell};
use std::collections::VecDeque;
use std::{future::Future, io, sync::Arc, thread};

use async_task::Runnable;
use crossbeam_queue::SegQueue;
use swap_buffer_queue::{Queue, buffer::ArrayBuffer, error::TryEnqueueError};

use crate::{driver::Driver, driver::Notify, driver::PollResult, handle::JoinHandle};

scoped_tls::scoped_thread_local!(static CURRENT_RUNTIME: Runtime);

/// The async runtime for ntex
///
/// It is a thread local runtime, and cannot be sent to other threads.
pub struct Runtime {
    stop: Cell<bool>,
    queue: Arc<RunnableQueue>,
}

impl Runtime {
    /// Create [`Runtime`] with default config.
    pub fn new(handle: Box<dyn Notify>) -> Self {
        Self::builder().build(handle)
    }

    /// Create a builder for [`Runtime`].
    pub fn builder() -> RuntimeBuilder {
        RuntimeBuilder::new()
    }

    #[allow(clippy::arc_with_non_send_sync)]
    fn with_builder(builder: &RuntimeBuilder, handle: Box<dyn Notify>) -> Self {
        Self {
            stop: Cell::new(false),
            queue: Arc::new(RunnableQueue::new(builder.event_interval, handle)),
        }
    }

    /// Perform a function on the current runtime.
    ///
    /// ## Panics
    ///
    /// This method will panic if there are no running [`Runtime`].
    pub fn with_current<T, F: FnOnce(&Self) -> T>(f: F) -> T {
        #[cold]
        fn not_in_neon_runtime() -> ! {
            panic!("not in a neon runtime")
        }

        if CURRENT_RUNTIME.is_set() {
            CURRENT_RUNTIME.with(f)
        } else {
            not_in_neon_runtime()
        }
    }

    #[inline]
    /// Get handle for current runtime
    pub fn handle(&self) -> Handle {
        Handle {
            queue: self.queue.clone(),
        }
    }

    /// Spawns a new asynchronous task, returning a [`Task`] for it.
    ///
    /// Spawning a task enables the task to execute concurrently to other tasks.
    /// There is no guarantee that a spawned task will execute to completion.
    pub fn spawn<F: Future + 'static>(&self, future: F) -> JoinHandle<F::Output> {
        unsafe { self.spawn_unchecked(future) }
    }

    /// Spawns a new asynchronous task, returning a [`Task`] for it.
    ///
    /// # Safety
    ///
    /// The caller should ensure the captured lifetime is long enough.
    pub unsafe fn spawn_unchecked<F: Future>(&self, future: F) -> JoinHandle<F::Output> {
        let queue = self.queue.clone();
        let schedule = move |runnable| {
            queue.schedule(runnable);
        };
        let (runnable, task) = unsafe { async_task::spawn_unchecked(future, schedule) };
        runnable.schedule();
        JoinHandle::new(task)
    }

    /// Poll runtime and run active tasks
    pub fn poll(&self) -> PollResult {
        if self.stop.get() {
            PollResult::Ready
        } else if self.queue.run() {
            PollResult::PollAgain
        } else {
            PollResult::Pending
        }
    }

    /// Runs the provided future, blocking the current thread until the future
    /// completes
    pub fn block_on<F: Future>(&self, future: F, driver: &dyn Driver) -> F::Output {
        self.stop.set(false);

        CURRENT_RUNTIME.set(self, || {
            let mut result = None;
            unsafe {
                self.spawn_unchecked(async {
                    result = Some(future.await);
                    self.stop.set(true);
                    let _ = self.queue.handle.notify();
                });
            }

            driver.run(self).expect("Driver failed");
            result.expect("Driver failed to poll")
        })
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        CURRENT_RUNTIME.set(self, || {
            self.queue.clear();
        })
    }
}

#[derive(Debug)]
/// Handle for current runtime
pub struct Handle {
    queue: Arc<RunnableQueue>,
}

impl Handle {
    /// Get handle for current runtime
    ///
    /// Panics if runtime is not set
    pub fn current() -> Handle {
        Runtime::with_current(|rt| rt.handle())
    }

    /// Wake up runtime
    pub fn notify(&self) -> io::Result<()> {
        self.queue.handle.notify()
    }

    /// Spawns a new asynchronous task, returning a [`Task`] for it.
    ///
    /// Spawning a task enables the task to execute concurrently to other tasks.
    /// There is no guarantee that a spawned task will execute to completion.
    pub fn spawn<F: Future + Send + 'static>(&self, future: F) -> JoinHandle<F::Output> {
        let queue = self.queue.clone();
        let schedule = move |runnable| {
            queue.schedule(runnable);
        };
        let (runnable, task) = unsafe { async_task::spawn_unchecked(future, schedule) };
        runnable.schedule();
        JoinHandle::new(task)
    }
}

impl Clone for Handle {
    fn clone(&self) -> Self {
        Self {
            queue: self.queue.clone(),
        }
    }
}

#[derive(Debug)]
struct RunnableQueue {
    id: thread::ThreadId,
    idle: Cell<bool>,
    handle: Box<dyn Notify>,
    event_interval: usize,
    local_queue: UnsafeCell<VecDeque<Runnable>>,
    sync_fixed_queue: Queue<ArrayBuffer<Runnable, 128>>,
    sync_queue: SegQueue<Runnable>,
}

unsafe impl Send for RunnableQueue {}
unsafe impl Sync for RunnableQueue {}

impl RunnableQueue {
    fn new(event_interval: usize, handle: Box<dyn Notify>) -> Self {
        Self {
            handle,
            event_interval,
            id: thread::current().id(),
            idle: Cell::new(true),
            local_queue: UnsafeCell::new(VecDeque::new()),
            sync_fixed_queue: Queue::default(),
            sync_queue: SegQueue::new(),
        }
    }

    fn schedule(&self, runnable: Runnable) {
        if self.id == thread::current().id() {
            unsafe { (*self.local_queue.get()).push_back(runnable) };
            if self.idle.get() {
                self.idle.set(false);
                self.handle.notify().ok();
            }
        } else {
            let result = self.sync_fixed_queue.try_enqueue([runnable]);
            if let Err(TryEnqueueError::InsufficientCapacity([runnable])) = result {
                self.sync_queue.push(runnable);
            }
            self.handle.notify().ok();
        }
    }

    fn run(&self) -> bool {
        for _ in 0..self.event_interval {
            let task = unsafe { (*self.local_queue.get()).pop_front() };
            if let Some(task) = task {
                task.run();
            } else {
                break;
            }
        }

        if let Ok(buf) = self.sync_fixed_queue.try_dequeue() {
            for task in buf {
                task.run();
            }
        }

        for _ in 0..self.event_interval {
            if !self.sync_queue.is_empty() {
                if let Some(task) = self.sync_queue.pop() {
                    task.run();
                    continue;
                }
            }
            break;
        }

        let more_tasks = !unsafe { (*self.local_queue.get()).is_empty() }
            || !self.sync_fixed_queue.is_empty()
            || !self.sync_queue.is_empty();

        if !more_tasks {
            self.idle.set(true);
        }
        more_tasks
    }

    fn clear(&self) {
        while self.sync_queue.pop().is_some() {}
        while self.sync_fixed_queue.try_dequeue().is_ok() {}
        unsafe { (*self.local_queue.get()).clear() };
    }
}

/// Builder for [`Runtime`].
#[derive(Debug, Clone)]
pub struct RuntimeBuilder {
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
        Self { event_interval: 61 }
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
    pub fn build(&self, handle: Box<dyn Notify>) -> Runtime {
        Runtime::with_builder(self, handle)
    }
}
