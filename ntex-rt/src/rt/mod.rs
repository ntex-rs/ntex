use std::cell::{Cell, RefCell};
use std::future::poll_fn;
use std::sync::atomic::Ordering;
use std::task::Poll;
use std::time::{Duration, Instant};
use std::{future::Future, io, marker::PhantomData, panic::Location};
use std::{pin::Pin, rc::Rc, sync::Arc};

use crate::driver::{Driver, Notify, PollResult};
use crate::handle::{JoinHandle, RemoteJoinHandle, SpawnLocation};
use crate::metrics::Histogram;
use crate::remote::{Job, RemoteJoin, Runner};

mod queue;
pub(crate) mod task;

use self::queue::RunnableQueue;

/// Log target of the scheduler, also used by its submodules.
const LOG_TARGET: &str = module_path!();

// marks a reported self-wake streak
pub(crate) const REPORTED: u32 = 1 << 31;

/// Self-wake bookkeeping of a task, used on the runtime thread only.
pub(crate) trait TaskStats: Copy {
    /// Returns the task's spawn location for diagnostics.
    fn location(self) -> SpawnLocation;
    /// Consecutive self-wakes, `REPORTED` marks a reported streak.
    fn self_wakes(self) -> u32;
    /// Stores the self-wake count and its `REPORTED` marker.
    fn set_self_wakes(self, wakes: u32);
    /// Start of the self-wake streak, milliseconds since the runtime epoch.
    fn streak_start(self) -> u32;
    /// Records the streak's start in milliseconds since the runtime epoch.
    fn set_streak_start(self, start: u32);
}

scoped_tls::scoped_thread_local!(static CURRENT_RUNTIME: Runtime);

/// Index of a thread that owns a runtime, never reused within the process.
#[cfg(not(ntex_loom))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ThreadId(std::num::NonZeroU16);

// loom threads share the OS thread, and its thread locals
#[cfg(ntex_loom)]
pub(crate) type ThreadId = loom::thread::ThreadId;

/// The last issued thread index.
#[cfg(not(ntex_loom))]
static THREAD_INDEX: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);

#[cfg(not(ntex_loom))]
thread_local!(static THREAD_ID: Cell<Option<ThreadId>> = const { Cell::new(None) });

/// Returns the index of the current thread, assigning one on first use.
///
/// Only threads that own a runtime call this, indexes are limited to
/// `u16::MAX` per process and the process aborts when they run out.
#[cfg(not(ntex_loom))]
#[inline]
pub(crate) fn current_thread_id() -> ThreadId {
    /// Assigns a never-reused index on the thread's first use.
    #[cold]
    fn initialize(slot: &Cell<Option<ThreadId>>) -> ThreadId {
        let id = THREAD_INDEX
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .ok()
            .and_then(|n| std::num::NonZeroU16::new(n + 1))
            .unwrap_or_else(|| {
                log::error!(target: LOG_TARGET, "Thread indexes are exhausted, {} runtime-owning threads per process", u16::MAX);
                std::process::abort()
            });
        let id = ThreadId(id);
        slot.set(Some(id));
        id
    }

    THREAD_ID.with(|slot| slot.get().unwrap_or_else(|| initialize(slot)))
}

/// Returns the index of the current thread without assigning one.
#[cfg(not(ntex_loom))]
#[inline]
pub(crate) fn try_current_thread_id() -> Option<ThreadId> {
    THREAD_ID.with(Cell::get)
}

/// Sets the last issued thread index, to test exhaustion.
#[cfg(all(test, not(ntex_loom)))]
pub(crate) fn set_thread_index(last: u16) {
    THREAD_INDEX.store(last, Ordering::Relaxed);
}

/// Returns the current logical Loom thread's identity.
#[cfg(ntex_loom)]
pub(crate) fn current_thread_id() -> ThreadId {
    loom::thread::current().id()
}

/// Returns the current logical Loom thread's identity.
#[cfg(ntex_loom)]
pub(crate) fn try_current_thread_id() -> Option<ThreadId> {
    Some(loom::thread::current().id())
}

#[derive(Debug)]
/// The async runtime for ntex.
///
/// This is a thread-local runtime and cannot be sent to other threads, use
/// [`Handle`] to spawn tasks from other threads.
///
/// Dropping the runtime cancels its tasks. If cleanup panics, shutdown finishes
/// before the panic is resumed. A second cleanup panic, or a cleanup panic
/// while the runtime is dropped during an existing unwind, aborts the process.
///
/// A process can create at most 65,535 runtimes, and at most 65,535 threads can
/// create one. Neither is reused, and the process aborts when either runs out.
///
/// ```compile_fail
/// fn assert_send<T: Send>() {}
/// assert_send::<ntex_rt::Runtime>();
/// ```
pub struct Runtime {
    stop: Cell<bool>,
    queue: Arc<RunnableQueue>,
    // tasks are polled and dropped on the thread that created the runtime
    _not_send: PhantomData<*const ()>,
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

    /// Creates the owner-thread queue and registers its remote wake endpoint.
    #[allow(clippy::arc_with_non_send_sync)]
    fn with_builder(builder: &RuntimeBuilder, handle: Box<dyn Notify>) -> Self {
        let queue = Arc::new(RunnableQueue::new(builder, handle));
        task::wake::register(&queue);
        Self {
            stop: Cell::new(false),
            queue,
            _not_send: PhantomData,
        }
    }

    /// Runs a closure with the runtime active on the current thread.
    ///
    /// ## Panics
    ///
    /// Panics if no runtime is active on the current thread.
    pub fn with_current<T, F: FnOnce(&Self) -> T>(f: F) -> T {
        /// Reports an operation that requires an active runtime.
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

    /// Returns [`Runtime::now`] of the runtime active on the current thread,
    /// or [`Instant::now()`] if no runtime is active.
    pub(crate) fn current_now() -> Instant {
        if CURRENT_RUNTIME.is_set() {
            CURRENT_RUNTIME.with(Runtime::now)
        } else {
            Instant::now()
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
    #[track_caller]
    pub fn spawn<F: Future + 'static>(&self, future: F) -> JoinHandle<F::Output> {
        unsafe { self.spawn_at(future, Location::caller()) }
    }

    /// Spawns a new asynchronous task, returning a [`JoinHandle`] for it.
    ///
    /// # Safety
    ///
    /// The caller should ensure the captured lifetime is long enough.
    #[track_caller]
    pub unsafe fn spawn_unchecked<F: Future>(&self, future: F) -> JoinHandle<F::Output> {
        unsafe { self.spawn_at(future, Location::caller()) }
    }

    /// Spawns a task, `location` is reported by [`Handle::current_task`].
    ///
    /// # Safety
    ///
    /// The caller should ensure the captured lifetime is long enough.
    pub(crate) unsafe fn spawn_at<F: Future>(
        &self,
        future: F,
        location: SpawnLocation,
    ) -> JoinHandle<F::Output> {
        unsafe { self.queue.spawn(future, location) }
    }

    /// Spawns a new asynchronous task without a [`JoinHandle`].
    ///
    /// The output is dropped when the task completes.
    #[track_caller]
    pub fn spawn_detached<F: Future + 'static>(&self, future: F) {
        unsafe { self.spawn_detached_at(future, Location::caller()) }
    }

    /// Spawns a detached task, `location` is reported by
    /// [`Handle::current_task`].
    ///
    /// # Safety
    ///
    /// The caller should ensure the captured lifetime is long enough.
    pub(crate) unsafe fn spawn_detached_at<F: Future>(&self, future: F, location: SpawnLocation) {
        unsafe { self.queue.spawn_detached::<_, true>(future, location) }
    }

    /// Marks the runtime as awake.
    ///
    /// Drivers call this after returning from a blocking wait, before handling
    /// events that wake tasks. Tasks scheduled from the runtime thread do not
    /// notify the driver until [`poll`](Self::poll) finds no more tasks to run,
    /// the driver must call `poll` before blocking again.
    ///
    /// Starts a busy period, see [`busy_duration`](Self::busy_duration).
    #[inline]
    pub fn awake(&self) {
        if self.queue.idle.get() {
            self.queue.start_busy();
        }
    }

    /// Polls the runtime and runs scheduled tasks.
    pub fn poll(&self) -> PollResult {
        let start = Instant::now();
        // tasks scheduled from other threads do not end the idle state
        if self.queue.idle.get() {
            self.queue.busy.start(start);
        }

        let result = if self.stop.get() {
            PollResult::Ready
        } else if self.queue.run() {
            self.queue.add_poll(true);
            PollResult::PollAgain
        } else {
            self.queue.add_poll(false);
            PollResult::Pending
        };

        // a runtime that never idles stays visible, its busy period is read
        // from its start
        let now = Instant::now();
        if result != PollResult::Ready {
            self.queue
                .poll_histogram
                .record(now.saturating_duration_since(start));
        }
        if result == PollResult::PollAgain {
            self.queue.set_poll_start(now);
            self.queue.busy.next_turn(now);
        } else {
            self.queue.idle.set(true);
            self.queue.busy.stop(now);
        }
        result
    }

    /// Returns the start of the current runtime turn.
    ///
    /// While the runtime is busy, returns the time the driver woke up or the
    /// previous [`poll`](Self::poll) finished, tasks polled in the same turn
    /// observe the same instant. Returns [`Instant::now()`] while the runtime
    /// is idle.
    #[inline]
    pub fn now(&self) -> Instant {
        if self.queue.idle.get() {
            Instant::now()
        } else {
            self.queue.poll_start.get()
        }
    }

    /// Returns the total time the runtime has been busy.
    ///
    /// The runtime is busy from [`awake`](Self::awake) until [`poll`](Self::poll)
    /// finds no more tasks to run. This includes running tasks and handling
    /// driver events, and excludes time the driver is blocked waiting for events.
    /// The value is wall-clock time, not CPU time, and includes the current
    /// busy period, so it can be read from other threads while a task runs.
    ///
    /// Utilization over an interval is the busy duration delta divided by the
    /// elapsed time of the interval.
    pub fn busy_duration(&self) -> Duration {
        self.queue.busy.busy_duration()
    }

    /// Returns how long the current runtime turn has been running, `None`
    /// while the runtime is idle.
    ///
    /// A turn ends when [`poll`](Self::poll) returns, a long turn means a task
    /// or a driver event handler keeps the runtime from making progress.
    pub fn current_turn(&self) -> Option<Duration> {
        self.queue.busy.current_turn()
    }

    /// Returns the number of times [`poll`](Self::poll) ran scheduled tasks.
    pub fn poll_count(&self) -> u64 {
        self.queue.polls.load(Ordering::Relaxed)
    }

    /// Returns the number of times [`poll`](Self::poll) returned
    /// [`PollResult::PollAgain`].
    ///
    /// `poll` runs a limited number of tasks, and returns `PollAgain` if
    /// scheduled tasks remain. The `poll_again_count` delta divided by the
    /// [`poll_count`](Self::poll_count) delta over an interval is the share of
    /// polls that left a backlog; a value close to 1 means tasks are scheduled
    /// faster than the runtime runs them.
    pub fn poll_again_count(&self) -> u64 {
        self.queue.polls_again.load(Ordering::Relaxed)
    }

    /// Returns the number of times a task woke itself while it was polled.
    ///
    /// A task that wakes itself, for example with `wake_by_ref()` or a yield,
    /// is scheduled again without waiting for an event. A steadily growing
    /// count points to tasks that poll in a loop, see
    /// [`RuntimeBuilder::self_wake_threshold`].
    pub fn self_wake_count(&self) -> u64 {
        self.queue.self_wakes.load(Ordering::Relaxed)
    }

    /// Returns the histogram of [`poll`](Self::poll) durations.
    ///
    /// A poll lasts from the start of the runtime turn, see [`now`](Self::now),
    /// until it finishes running tasks. Tasks woken during a poll wait at least
    /// until it finishes, long polls mean high scheduling latency.
    pub fn poll_histogram(&self) -> Histogram {
        self.queue.poll_histogram.snapshot()
    }

    /// Runs the provided future.
    ///
    /// Blocks the current thread until the future completes.
    ///
    /// # Panics
    ///
    /// Panics if the driver fails to run the provided future, or if a runtime
    /// is already active on the current thread. Nested calls would block the
    /// running task and its runtime until the inner future completes.
    #[track_caller]
    pub fn block_on<F: Future>(&self, future: F, driver: &dyn Driver) -> F::Output {
        /// Rejects blocking from within an already active runtime.
        #[cold]
        #[track_caller]
        fn nested_block_on() -> ! {
            panic!("Cannot start a runtime from within a runtime")
        }

        if CURRENT_RUNTIME.is_set() {
            nested_block_on()
        }

        let location = Location::caller();
        self.stop.set(false);
        self.queue.set_poll_start(Instant::now());

        CURRENT_RUNTIME.set(self, || {
            let mut result = None;

            // the task outlives this call if the driver fails or panics, it
            // reaches the borrowing future only through the slot, which the
            // guard empties before the borrows end
            let slot = BlockOnSlot(Rc::new(RefCell::new(Some(Box::pin(async {
                result = Some(future.await);
                self.stop.set(true);
                self.queue.notify();
            })))));
            let task = slot.0.clone();
            unsafe {
                self.spawn_detached_at(
                    poll_fn(move |cx| {
                        let mut fut = task.borrow_mut();
                        if let Some(f) = fut.as_mut()
                            && f.as_mut().poll(cx).is_pending()
                        {
                            return Poll::Pending;
                        }
                        *fut = None;
                        Poll::Ready(())
                    }),
                    location,
                );
            }

            ntex_error::set_backtrace_start_alt("src/rt/task.rs", 0);
            let res = driver.run(self);
            drop(slot);
            res.expect("Driver failed");
            result.expect("Driver failed to poll")
        })
    }
}

/// Drops the [`Runtime::block_on`] future when the call returns or unwinds.
struct BlockOnSlot<F>(Rc<RefCell<Option<Pin<Box<F>>>>>);

impl<F> Drop for BlockOnSlot<F> {
    /// Removes the borrowing future before the `block_on` scope ends.
    fn drop(&mut self) {
        let fut = self.0.borrow_mut().take();
        drop(fut);
    }
}

impl Drop for Runtime {
    /// Cancels outstanding tasks with this runtime active during cleanup.
    fn drop(&mut self) {
        CURRENT_RUNTIME.set(self, || {
            self.queue.shutdown();
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
    ///
    /// Returns notification errors to the caller without retrying, unlike
    /// internal scheduler notifications.
    pub fn notify(&self) -> io::Result<()> {
        self.queue.handle.notify()
    }

    /// Returns the total time the runtime has been busy.
    ///
    /// See [`Runtime::busy_duration`]. Always returns `Some`, the return type
    /// matches the other runtime backends.
    pub fn busy_duration(&self) -> Option<Duration> {
        Some(self.queue.busy.busy_duration())
    }

    /// Returns how long the current runtime turn has been running, `None`
    /// while the runtime is idle.
    ///
    /// See [`Runtime::current_turn`].
    pub fn current_turn(&self) -> Option<Duration> {
        self.queue.busy.current_turn()
    }

    /// Returns the number of runtime polls that ran scheduled tasks.
    ///
    /// See [`Runtime::poll_count`]. Always returns `Some`.
    pub fn poll_count(&self) -> Option<u64> {
        Some(self.queue.polls.load(Ordering::Relaxed))
    }

    /// Returns the number of runtime polls that left scheduled tasks.
    ///
    /// See [`Runtime::poll_again_count`]. Always returns `Some`.
    pub fn poll_again_count(&self) -> Option<u64> {
        Some(self.queue.polls_again.load(Ordering::Relaxed))
    }

    /// Returns the number of times a task woke itself while it was polled.
    ///
    /// See [`Runtime::self_wake_count`]. Always returns `Some`.
    pub fn self_wake_count(&self) -> Option<u64> {
        Some(self.queue.self_wakes.load(Ordering::Relaxed))
    }

    /// Returns the spawn location of the task the runtime is polling.
    ///
    /// Returns `None` while the runtime is idle, handles driver events or
    /// between tasks. Can be read from other threads, for example to name the
    /// task that blocks a stalled runtime.
    pub fn current_task(&self) -> Option<&'static Location<'static>> {
        let task = self.queue.current.load(Ordering::Relaxed);
        // only null or a `&'static Location` is stored
        unsafe { task.as_ref() }
    }

    /// Returns the histogram of runtime poll durations.
    ///
    /// See [`Runtime::poll_histogram`]. Always returns `Some`.
    pub fn poll_histogram(&self) -> Option<Histogram> {
        Some(self.queue.poll_histogram.snapshot())
    }

    /// Spawns a task on the handle's runtime and returns a [`RemoteJoinHandle`].
    ///
    /// Spawning a task enables the task to execute concurrently to other tasks.
    /// There is no guarantee that a spawned task will execute to completion.
    ///
    /// On the runtime thread the task is spawned directly. From other threads
    /// the future and output must be `Send`, the future is polled and dropped
    /// on the runtime thread and its output is returned through the handle.
    /// The handle is neither `Send` nor `Sync`, dropping it detaches the task.
    ///
    /// ```no_run
    /// # async fn check(hnd: ntex_rt::Handle) {
    /// let task = hnd.spawn(async { 42 });
    /// assert_eq!(task.await.unwrap(), 42);
    /// # }
    /// ```
    #[track_caller]
    pub fn spawn<F>(&self, future: F) -> RemoteJoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let location = Location::caller();
        if Some(self.queue.id) == try_current_thread_id() {
            RemoteJoinHandle::local(unsafe { self.queue.spawn(future, location) })
        } else {
            let job = Job::new(future, true);
            let join = RemoteJoin::new(job.clone());
            // a rejected submission closes the job on this thread
            unsafe {
                self.queue.spawn_detached::<_, false>(Runner(job), location);
            }
            RemoteJoinHandle::remote(join)
        }
    }

    /// Spawns a new asynchronous task without a join handle.
    ///
    /// The output is dropped on the runtime thread when the task completes,
    /// so it does not have to be `Send`.
    #[track_caller]
    pub fn spawn_detached<F>(&self, future: F)
    where
        F: Future + Send + 'static,
    {
        unsafe {
            self.queue
                .spawn_detached::<_, false>(future, Location::caller());
        }
    }
}

impl Clone for Handle {
    /// Shares the runtime's queue without creating another runtime.
    fn clone(&self) -> Self {
        Self {
            queue: self.queue.clone(),
        }
    }
}

/// Builder for [`Runtime`].
#[derive(Debug, Clone)]
pub struct RuntimeBuilder {
    event_interval: usize,
    self_wake_limit: u32,
    self_wake_time: Duration,
}

impl Default for RuntimeBuilder {
    /// Uses the default scheduler budgets and self-wake thresholds.
    fn default() -> Self {
        Self::new()
    }
}

impl RuntimeBuilder {
    /// Create the builder with default config.
    pub fn new() -> Self {
        Self {
            event_interval: 61,
            self_wake_limit: 10_000,
            self_wake_time: Duration::from_secs(1),
        }
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

    /// Sets the self-wake streak that is reported as a possible busy loop.
    ///
    /// A task that wakes itself on every poll, for example a loop around
    /// `wake_by_ref()` or a yield, never waits for an event. The runtime logs
    /// a warning with the task spawn location once the task wakes itself
    /// `count` times in a row and the streak lasts at least `duration`. Each
    /// streak is reported once. A count of 0 disables the warning and per-task
    /// streak tracking, but preserves [`Runtime::self_wake_count`].
    ///
    /// By default, 10 000 self-wakes over at least 1 second.
    pub fn self_wake_threshold(&mut self, count: u32, duration: Duration) -> &mut Self {
        self.self_wake_limit = count.min(!REPORTED);
        self.self_wake_time = duration;
        self
    }

    /// Build [`Runtime`].
    pub fn build(&self, handle: Box<dyn Notify>) -> Runtime {
        Runtime::with_builder(self, handle)
    }
}

#[cfg(test)]
mod task_tests;

#[cfg(all(test, ntex_loom))]
mod task_loom;

#[cfg(test)]
mod tests;
