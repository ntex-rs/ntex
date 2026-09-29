use std::cell::{Cell, UnsafeCell};
use std::collections::VecDeque;
use std::{future::Future, io, sync::Arc, thread};

use async_task::Runnable;
use crossbeam_queue::SegQueue;
use swap_buffer_queue::error::{TryDequeueError, TryEnqueueError};
use swap_buffer_queue::{Queue, buffer::ArrayBuffer};

use crate::{driver::Driver, driver::Notify, driver::PollResult, handle::JoinHandle};

scoped_tls::scoped_thread_local!(static CURRENT_RUNTIME: Runtime);

#[derive(Debug)]
/// The async runtime for ntex.
///
/// This is a thread-local runtime and cannot be sent to other threads.
pub struct Runtime {
    stop: Cell<bool>,
    queue: Arc<RunnableQueue>,
}

impl Runtime {
    /// Creates a runtime with default configuration.
    pub fn new(handle: Box<dyn Notify>) -> Self {
        Self::builder().build(handle)
    }

    /// Creates a runtime builder.
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

    /// Runs a closure with the runtime active on the current thread.
    ///
    /// ## Panics
    ///
    /// Panics if no runtime is active on the current thread.
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
    /// Returns a handle to this runtime.
    pub fn handle(&self) -> Handle {
        Handle {
            queue: self.queue.clone(),
        }
    }

    /// Spawns a new asynchronous task, returning a [`JoinHandle`] for it.
    ///
    /// Spawning a task enables the task to execute concurrently to other tasks.
    /// There is no guarantee that a spawned task will execute to completion.
    pub fn spawn<F: Future + 'static>(&self, future: F) -> JoinHandle<F::Output> {
        unsafe { self.spawn_unchecked(future) }
    }

    /// Spawns a new asynchronous task, returning a [`JoinHandle`] for it.
    ///
    /// # Safety
    ///
    /// The caller should ensure the captured lifetime is long enough.
    pub unsafe fn spawn_unchecked<F: Future>(&self, future: F) -> JoinHandle<F::Output> {
        let queue = self.queue.clone();
        let (runnable, task) = unsafe {
            async_task::spawn_unchecked(future, move |runnable| {
                queue.schedule(runnable);
            })
        };
        runnable.schedule();
        JoinHandle::new(task)
    }

    /// Polls the runtime and runs scheduled tasks.
    pub fn poll(&self) -> PollResult {
        if self.stop.get() {
            PollResult::Ready
        } else if self.queue.run() {
            PollResult::PollAgain
        } else {
            PollResult::Pending
        }
    }

    /// Runs the provided future.
    ///
    /// Blocks the current thread until the future completes.
    ///
    /// # Panics
    ///
    /// Panics if the driver fails to run the provided future.
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

            ntex_error::set_backtrace_start_alt("src/raw.rs", 0);
            driver.run(self).expect("Driver failed");
            result.expect("Driver failed to poll")
        })
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        CURRENT_RUNTIME.set(self, || {
            self.queue.clear();
        });
    }
}

#[derive(Debug)]
/// A thread-safe handle used to schedule work on a runtime.
pub struct Handle {
    queue: Arc<RunnableQueue>,
}

impl Handle {
    /// Returns a handle to the runtime active on the current thread.
    ///
    /// # Panics
    ///
    /// Panics if no runtime is active on the current thread.
    pub fn current() -> Handle {
        Runtime::with_current(Runtime::handle)
    }

    /// Wakes the runtime's driver.
    pub fn notify(&self) -> io::Result<()> {
        self.queue.handle.notify()
    }

    /// Spawns a new asynchronous task, returning a [`JoinHandle`] for it.
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
        // a running task may schedule into `local_queue`, so it must not be
        // borrowed across `task.run()`
        let local_queue = {
            for _ in 0..self.event_interval {
                if let Some(task) = self.pop_local() {
                    task.run();
                } else {
                    break;
                }
            }
            unsafe { !(*self.local_queue.get()).is_empty() }
        };

        let sync_queue_fixed = match self.sync_fixed_queue.try_dequeue() {
            Ok(buf) => {
                for task in buf {
                    task.run();
                }
                false
            }
            Err(TryDequeueError::Empty | TryDequeueError::Closed) => false,
            Err(_) => true,
        };

        let sync_queue = {
            for _ in 0..self.event_interval {
                if let Some(task) = self.sync_queue.pop() {
                    task.run();
                } else {
                    break;
                }
            }
            !self.sync_queue.is_empty()
        };

        let more_tasks = local_queue || sync_queue_fixed || sync_queue;
        if !more_tasks {
            self.idle.set(true);
        }
        more_tasks
    }

    fn clear(&self) {
        while self.sync_queue.pop().is_some() {}
        while self.sync_fixed_queue.try_dequeue().is_ok() {}
        // dropped tasks may schedule other tasks, drop each outside of the borrow
        while let Some(task) = self.pop_local() {
            drop(task);
        }
    }

    fn pop_local(&self) -> Option<Runnable> {
        unsafe { (*self.local_queue.get()).pop_front() }
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
    /// Values below 1 are treated as 1.
    pub fn event_interval(&mut self, val: usize) -> &mut Self {
        self.event_interval = val.max(1);
        self
    }

    /// Build [`Runtime`].
    pub fn build(&self, handle: Box<dyn Notify>) -> Runtime {
        Runtime::with_builder(self, handle)
    }
}

#[cfg(test)]
mod tests {
    use std::task::{Poll, Waker};
    use std::{cell::RefCell, future::poll_fn, rc::Rc};

    use super::*;

    #[derive(Debug)]
    struct NoopNotify;

    impl Notify for NoopNotify {
        fn notify(&self) -> io::Result<()> {
            Ok(())
        }
    }

    struct WakeOnDrop(Rc<RefCell<Option<Waker>>>);

    impl Drop for WakeOnDrop {
        fn drop(&mut self) {
            if let Some(w) = self.0.borrow_mut().take() {
                w.wake();
            }
        }
    }

    #[test]
    fn schedule_while_running() {
        let rt = Runtime::new(Box::new(NoopNotify));
        let done = Rc::new(RefCell::new(0));
        let done2 = done.clone();
        rt.spawn(async move {
            // schedules into the local queue from a running task
            let h = Runtime::with_current(|rt| rt.spawn(async { 1 }));
            *done2.borrow_mut() = h.await.unwrap();
        })
        .detach();
        CURRENT_RUNTIME.set(&rt, || while rt.poll() == PollResult::PollAgain {});
        assert_eq!(*done.borrow(), 1);
    }

    #[test]
    fn event_interval() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        for val in [0, 1] {
            let rt = Runtime::builder()
                .event_interval(val)
                .build(Box::new(NoopNotify));
            assert_eq!(rt.poll(), PollResult::Pending);

            rt.spawn(async {}).detach();
            rt.spawn(async {}).detach();
            assert_eq!(rt.poll(), PollResult::PollAgain);
            assert_eq!(rt.poll(), PollResult::Pending);
        }

        // tasks scheduled from other threads overflow the fixed queue
        let rt = Runtime::builder()
            .event_interval(1)
            .build(Box::new(NoopNotify));
        let cnt = Arc::new(AtomicUsize::new(0));
        let hnd = rt.handle();
        let cnt2 = cnt.clone();
        std::thread::spawn(move || {
            for _ in 0..130 {
                let cnt = cnt2.clone();
                hnd.spawn(async move {
                    cnt.fetch_add(1, Ordering::Relaxed);
                })
                .detach();
            }
        })
        .join()
        .unwrap();
        assert_eq!(rt.poll(), PollResult::PollAgain);
        assert_eq!(cnt.load(Ordering::Relaxed), 129);
        assert_eq!(rt.poll(), PollResult::Pending);
        assert_eq!(cnt.load(Ordering::Relaxed), 130);
    }

    #[test]
    fn schedule_while_clearing() {
        let rt = Runtime::new(Box::new(NoopNotify));
        let waker = Rc::new(RefCell::new(None));
        let waker2 = waker.clone();
        rt.spawn(poll_fn(move |cx| {
            *waker2.borrow_mut() = Some(cx.waker().clone());
            Poll::<()>::Pending
        }))
        .detach();
        assert_eq!(rt.poll(), PollResult::Pending);
        assert!(waker.borrow().is_some());

        // queued task wakes the waiting task while the queue is cleared
        let guard = WakeOnDrop(waker.clone());
        rt.spawn(async move {
            let _g = guard;
        })
        .detach();
        rt.spawn(async {}).detach();
        drop(rt);
        assert!(waker.borrow().is_none());
        // the woken task is dropped too, releasing its clone of `waker`
        assert_eq!(Rc::strong_count(&waker), 1);
    }
}
