use std::cell::{Cell, RefCell};
use std::future::poll_fn;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
use std::task::Poll;
use std::time::{Duration, Instant};
use std::{future::Future, io, marker::PhantomData, panic::Location, ptr, thread};
use std::{pin::Pin, rc::Rc, sync::Arc};

use crate::handle::{JoinHandle, SpawnLocation};
use crate::metrics::{AtomicHistogram, Histogram};
use crate::{busy::BusyClock, driver::Driver, driver::Notify, driver::PollResult};

#[cfg(not(ntex_rt_task))]
use self::queues::{LocalQueue, RemoteQueue, RemoteTask, Task, TaskMeta, task_ref};
#[cfg(ntex_rt_task)]
use crate::raw::{
    LocalQueue, Remote as RemoteTask, RemoteQueue, Runnable as Task, TaskList, task_ref,
};

// marks a reported self-wake streak
pub(crate) const REPORTED: u32 = 1 << 31;

#[cfg(not(ntex_rt_task))]
const MAX_SELF_WAKES: u32 = !REPORTED;
#[cfg(ntex_rt_task)]
const MAX_SELF_WAKES: u32 = crate::raw::MAX_SELF_WAKES;

/// Remote tasks run per turn, besides `event_interval` overflow tasks.
const REMOTE_BATCH: usize = 128;

/// Self-wake bookkeeping of a task, used on the runtime thread only.
pub(crate) trait TaskStats: Copy {
    fn location(self) -> SpawnLocation;
    /// Consecutive self-wakes, `REPORTED` marks a reported streak.
    fn self_wakes(self) -> u32;
    fn set_self_wakes(self, wakes: u32);
    /// Start of the self-wake streak, milliseconds since the runtime epoch.
    fn streak_start(self) -> u32;
    fn set_streak_start(self, start: u32);
}

scoped_tls::scoped_thread_local!(static CURRENT_RUNTIME: Runtime);

#[cfg(not(ntex_loom))]
pub(crate) type ThreadId = thread::ThreadId;

// loom threads share the OS thread, and its thread locals
#[cfg(ntex_loom)]
pub(crate) type ThreadId = loom::thread::ThreadId;

#[cfg(not(ntex_loom))]
thread_local!(static THREAD_ID: Cell<Option<ThreadId>> = const { Cell::new(None) });

/// Returns the cached id of the current thread.
#[cfg(not(ntex_loom))]
#[inline]
pub(crate) fn current_thread_id() -> ThreadId {
    #[cold]
    fn initialize(slot: &Cell<Option<ThreadId>>) -> ThreadId {
        let id = thread::current().id();
        slot.set(Some(id));
        id
    }

    THREAD_ID.with(|slot| slot.get().unwrap_or_else(|| initialize(slot)))
}

#[cfg(ntex_loom)]
pub(crate) fn current_thread_id() -> ThreadId {
    loom::thread::current().id()
}

#[derive(Debug)]
/// The async runtime for ntex.
///
/// This is a thread-local runtime and cannot be sent to other threads, use
/// [`Handle`] to spawn tasks from other threads.
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

    #[allow(clippy::arc_with_non_send_sync)]
    fn with_builder(builder: &RuntimeBuilder, handle: Box<dyn Notify>) -> Self {
        Self {
            stop: Cell::new(false),
            queue: Arc::new(RunnableQueue::new(builder, handle)),
            _not_send: PhantomData,
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
        unsafe { self.queue.spawn_detached(future, location) }
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
            self.queue.poll_start.set(now);
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
        self.queue.poll_start.set(Instant::now());

        CURRENT_RUNTIME.set(self, || {
            let mut result = None;

            // the task outlives this call if the driver fails or panics, it
            // reaches the borrowing future only through the slot, which the
            // guard empties before the borrows end
            let slot = BlockOnSlot(Rc::new(RefCell::new(Some(Box::pin(async {
                result = Some(future.await);
                self.stop.set(true);
                let _ = self.queue.handle.notify();
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

            ntex_error::set_backtrace_start_alt("src/raw.rs", 0);
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
    fn drop(&mut self) {
        let fut = self.0.borrow_mut().take();
        drop(fut);
    }
}

impl Drop for Runtime {
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

    /// Spawns a new asynchronous task, returning a [`JoinHandle`] for it.
    ///
    /// Spawning a task enables the task to execute concurrently to other tasks.
    /// There is no guarantee that a spawned task will execute to completion.
    ///
    /// The output is moved to the thread that awaits the [`JoinHandle`], so
    /// it must be `Send`. Use [`Runtime::spawn`] for non-`Send` outputs.
    ///
    /// ```compile_fail
    /// # fn check(hnd: ntex_rt::Handle) {
    /// let _ = hnd.spawn(async { std::rc::Rc::new(()) });
    /// # }
    /// ```
    #[track_caller]
    pub fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send,
    {
        unsafe { self.queue.spawn(future, Location::caller()) }
    }

    /// Spawns a new asynchronous task without a [`JoinHandle`].
    ///
    /// The output is dropped on the runtime thread when the task completes,
    /// so it does not have to be `Send`.
    #[track_caller]
    pub fn spawn_detached<F>(&self, future: F)
    where
        F: Future + Send + 'static,
    {
        unsafe { self.queue.spawn_detached(future, Location::caller()) }
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
#[repr(C)]
pub(crate) struct RunnableQueue {
    // read-only, other threads read `id` on every waker operation
    pub(crate) id: ThreadId,
    handle: Box<dyn Notify>,
    epoch: Instant,
    self_wake_time: Duration,
    event_interval: usize,
    self_wake_limit: u32,
    // written by other threads
    _remote: [CacheLine; 0],
    // set by the first remote `schedule` after `run` cleared it
    notified: AtomicBool,
    remote_queue: RemoteQueue,
    // written by the runtime thread
    _local: [CacheLine; 0],
    idle: Cell<bool>,
    // start of the current busy period, valid while not idle
    poll_start: Cell<Instant>,
    busy: BusyClock,
    // poll counters, written by the runtime thread only
    polls: AtomicU64,
    polls_again: AtomicU64,
    poll_histogram: AtomicHistogram,
    // self-wake counter, written by the runtime thread only
    self_wakes: AtomicU64,
    // spawn location of the running task, null between tasks
    current: AtomicPtr<Location<'static>>,
    local_queue: LocalQueue,
    // unfinished tasks, dropped with the runtime
    #[cfg(ntex_rt_task)]
    pub(crate) tasks: TaskList,
}

/// Aligns the next field to a cache line, 128 bytes covers the adjacent line
/// prefetcher on x86 and the line size on Apple cores.
#[derive(Debug)]
#[repr(align(128))]
struct CacheLine;

unsafe impl Send for RunnableQueue {}
unsafe impl Sync for RunnableQueue {}

impl RunnableQueue {
    fn new(builder: &RuntimeBuilder, handle: Box<dyn Notify>) -> Self {
        let now = Instant::now();
        Self {
            handle,
            event_interval: builder.event_interval,
            id: current_thread_id(),
            idle: Cell::new(true),
            poll_start: Cell::new(now),
            notified: AtomicBool::new(false),
            busy: BusyClock::new(),
            polls: AtomicU64::new(0),
            polls_again: AtomicU64::new(0),
            poll_histogram: AtomicHistogram::default(),
            self_wakes: AtomicU64::new(0),
            self_wake_limit: builder.self_wake_limit,
            self_wake_time: builder.self_wake_time,
            epoch: now,
            current: AtomicPtr::new(ptr::null_mut()),
            local_queue: LocalQueue::new(),
            #[cfg(ntex_rt_task)]
            tasks: TaskList::new(),
            remote_queue: RemoteQueue::new(),
            _remote: [],
            _local: [],
        }
    }

    /// Spawns a task on this queue.
    ///
    /// # Safety
    ///
    /// The caller should ensure the captured lifetime is long enough.
    #[cfg(ntex_rt_task)]
    unsafe fn spawn<F: Future>(
        self: &Arc<Self>,
        future: F,
        location: SpawnLocation,
    ) -> JoinHandle<F::Output> {
        JoinHandle::new(unsafe { crate::raw::spawn(future, self, location) })
    }

    /// Spawns a task on this queue without a join handle.
    ///
    /// # Safety
    ///
    /// The caller should ensure the captured lifetime is long enough.
    #[cfg(ntex_rt_task)]
    unsafe fn spawn_detached<F: Future>(self: &Arc<Self>, future: F, location: SpawnLocation) {
        unsafe { crate::raw::spawn_detached(future, self, location) }
    }

    /// Spawns a task on this queue without a join handle.
    ///
    /// # Safety
    ///
    /// The caller should ensure the captured lifetime is long enough.
    #[cfg(not(ntex_rt_task))]
    unsafe fn spawn_detached<F: Future>(self: &Arc<Self>, future: F, location: SpawnLocation) {
        // the output is dropped by the task, async-task drops the output of
        // a finished task on the detaching thread
        unsafe {
            self.spawn(
                async move {
                    future.await;
                },
                location,
            )
        }
        .detach();
    }

    /// Spawns a task on this queue.
    ///
    /// # Safety
    ///
    /// The caller should ensure the captured lifetime is long enough.
    #[cfg(not(ntex_rt_task))]
    unsafe fn spawn<F: Future>(
        self: &Arc<Self>,
        future: F,
        location: SpawnLocation,
    ) -> JoinHandle<F::Output> {
        // a zero-sized schedule function, async-task keeps a task reference
        // around each call of a schedule function that captures state
        fn schedule(runnable: Task) {
            let queue = &runnable.metadata().queue;
            if queue.id == current_thread_id() {
                // the runtime thread cannot drop the queue during this call
                let queue: *const RunnableQueue = Arc::as_ptr(queue);
                unsafe { (*queue).schedule_local(runnable) };
            } else {
                // the runtime may run and drop the task as soon as it is
                // queued, the queue must outlive the call
                let queue = queue.clone();
                queue.schedule_remote(runnable);
            }
        }

        let (runnable, task) = unsafe {
            async_task::Builder::new()
                .metadata(TaskMeta::new(self.clone(), location))
                .spawn_unchecked(move |_| future, schedule)
        };
        self.schedule(runnable);
        JoinHandle::new(task)
    }

    #[cfg(not(ntex_rt_task))]
    fn schedule(&self, runnable: Task) {
        if self.id == current_thread_id() {
            self.schedule_local(runnable);
        } else {
            self.schedule_remote(runnable);
        }
    }

    pub(crate) fn schedule_local(&self, runnable: Task) {
        self.local_queue.push(runnable);
        if self.idle.get() {
            // starts a busy period, `awake` does not reset it
            self.start_busy();
            // the poll running a task picks up tasks it schedules, a notify
            // could reenter the runtime and free the running task
            if self.current.load(Ordering::Relaxed).is_null() {
                self.handle.notify().ok();
            }
        }
    }

    pub(crate) fn schedule_remote(&self, runnable: RemoteTask) {
        self.remote_queue.push(runnable);
        // the driver is already notified, `run` drains this task after
        // clearing the flag
        if !self.notified.swap(true, Ordering::Release) {
            self.handle.notify().ok();
        }
    }

    fn run(&self) -> bool {
        // cleared before draining the sync queues, a task enqueued after the
        // drain notifies the driver again
        if self.notified.load(Ordering::Relaxed) {
            self.notified.swap(false, Ordering::Acquire);
        }

        // a running task may schedule into `local_queue`, so it must not be
        // borrowed across `task.run()`
        for _ in 0..self.event_interval {
            if let Some(task) = self.pop_local() {
                self.run_task(task);
            } else {
                break;
            }
        }

        let remote_queue = self
            .remote_queue
            .drain(self.event_interval, |task| self.run_remote(task));

        // tasks from other threads may schedule local tasks
        let more_tasks = !self.local_queue.is_empty() || remote_queue;
        if !more_tasks {
            self.idle.set(true);
        }
        more_tasks
    }

    #[cfg(not(ntex_rt_task))]
    fn run_remote(&self, task: RemoteTask) {
        self.run_task(task);
    }

    #[cfg(ntex_rt_task)]
    fn run_remote(&self, task: RemoteTask) {
        if let Some(task) = task.into_runnable() {
            self.run_task(task);
        }
    }

    fn run_task(&self, task: Task) {
        struct Reset<'a>(&'a AtomicPtr<Location<'static>>, *mut Location<'static>);

        impl Drop for Reset<'_> {
            fn drop(&mut self) {
                // a task that polls the runtime runs tasks inside its own run
                self.0.store(self.1, Ordering::Relaxed);
            }
        }

        let stats = task_ref(&task);
        let prev = self.current.swap(
            ptr::from_ref(stats.location()).cast_mut(),
            Ordering::Relaxed,
        );
        let wakes = stats.self_wakes();
        if wakes != 0 {
            stats.set_self_wakes(0);
        }
        let _reset = Reset(&self.current, prev);

        if task.run() {
            // woken while polled, the task is alive in `local_queue`
            self.self_wake(stats, wakes);
        }
    }

    fn self_wake(&self, task: impl TaskStats, wakes: u32) {
        let total = self.self_wakes.load(Ordering::Relaxed);
        self.self_wakes
            .store(total.wrapping_add(1), Ordering::Relaxed);

        let reported = wakes & REPORTED;
        let count = ((wakes & !REPORTED) + 1).min(MAX_SELF_WAKES);
        // wraps after 49 days, longer streaks may be measured short
        #[allow(clippy::cast_possible_truncation)]
        let now = self
            .poll_start
            .get()
            .saturating_duration_since(self.epoch)
            .as_millis() as u32;
        if count == 1 {
            task.set_streak_start(now);
        }

        let mut wakes = count | reported;
        if reported == 0
            && self.self_wake_limit != 0
            && count >= self.self_wake_limit.min(MAX_SELF_WAKES)
        {
            let elapsed = Duration::from_millis(u64::from(now.wrapping_sub(task.streak_start())));
            if elapsed >= self.self_wake_time {
                log::warn!(
                    "Task spawned at {} woke itself {count} times in {elapsed:?} without waiting, possible busy loop",
                    task.location()
                );
                wakes |= REPORTED;
            }
        }
        task.set_self_wakes(wakes);
    }

    fn start_busy(&self) {
        let now = Instant::now();
        self.idle.set(false);
        self.poll_start.set(now);
        self.busy.start(now);
    }

    fn add_poll(&self, again: bool) {
        let polls = self.polls.load(Ordering::Relaxed);
        self.polls.store(polls.wrapping_add(1), Ordering::Relaxed);
        if again {
            let polls = self.polls_again.load(Ordering::Relaxed);
            self.polls_again
                .store(polls.wrapping_add(1), Ordering::Relaxed);
        }
    }

    fn clear(&self) {
        self.remote_queue.clear();
        // dropped tasks may schedule other tasks, drop each outside of the borrow
        while let Some(task) = self.pop_local() {
            drop(task);
        }
    }

    #[cfg(not(ntex_rt_task))]
    fn shutdown(&self) {
        self.clear();
    }

    /// Drops the futures of all tasks, also of tasks waiting on a waker.
    #[cfg(ntex_rt_task)]
    fn shutdown(&self) {
        // dropped futures may wake or spawn tasks
        loop {
            self.clear();
            if !self.tasks.close_first() {
                break;
            }
        }
    }

    fn pop_local(&self) -> Option<Task> {
        self.local_queue.pop()
    }
}

#[cfg(ntex_rt_task)]
impl RemoteQueue {
    /// Runs queued tasks, returns `true` if tasks are left.
    fn drain(&self, limit: usize, mut f: impl FnMut(RemoteTask)) -> bool {
        for _ in 0..limit + REMOTE_BATCH {
            match self.pop() {
                Ok(Some(task)) => f(task),
                Ok(None) => return false,
                Err(_) => return true,
            }
        }
        !self.is_empty()
    }

    fn clear(&self) {
        loop {
            match self.pop() {
                Ok(Some(task)) => drop(task),
                Ok(None) => break,
                // a push from another thread is in progress
                Err(_) => thread::yield_now(),
            }
        }
    }
}

#[cfg(not(ntex_rt_task))]
pub(crate) mod queues {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::{cell::UnsafeCell, collections::VecDeque, sync::Arc};

    use async_task::Runnable;
    use crossbeam_queue::SegQueue;
    use swap_buffer_queue::error::{TryDequeueError, TryEnqueueError};
    use swap_buffer_queue::{Queue, buffer::ArrayBuffer};

    use super::{REMOTE_BATCH, RunnableQueue, SpawnLocation, TaskStats};

    pub(crate) type Task = Runnable<TaskMeta>;
    pub(crate) type RemoteTask = Task;

    #[derive(Debug)]
    /// Task metadata.
    pub(crate) struct TaskMeta {
        /// Queue of the runtime the task belongs to, kept here instead of in
        /// the schedule function so that async-task does not clone a waker
        /// around each schedule.
        pub(crate) queue: Arc<RunnableQueue>,
        location: SpawnLocation,
        /// Consecutive polls that woke the task itself, the high bit marks a
        /// reported streak.
        self_wakes: AtomicU32,
        /// Start of the self-wake streak, milliseconds since the runtime epoch.
        streak_start: AtomicU32,
    }

    impl TaskMeta {
        pub(crate) fn new(queue: Arc<RunnableQueue>, location: SpawnLocation) -> Self {
            Self {
                queue,
                location,
                self_wakes: AtomicU32::new(0),
                streak_start: AtomicU32::new(0),
            }
        }
    }

    /// Metadata of a task, valid while the task is alive.
    #[derive(Clone, Copy)]
    pub(crate) struct MetaRef(*const TaskMeta);

    pub(crate) fn task_ref(task: &Task) -> MetaRef {
        MetaRef(task.metadata())
    }

    impl MetaRef {
        fn get(&self) -> &TaskMeta {
            unsafe { &*self.0 }
        }
    }

    impl TaskStats for MetaRef {
        fn location(self) -> SpawnLocation {
            self.get().location
        }

        fn self_wakes(self) -> u32 {
            self.get().self_wakes.load(Ordering::Relaxed)
        }

        fn set_self_wakes(self, wakes: u32) {
            self.get().self_wakes.store(wakes, Ordering::Relaxed);
        }

        fn streak_start(self) -> u32 {
            self.get().streak_start.load(Ordering::Relaxed)
        }

        fn set_streak_start(self, start: u32) {
            self.get().streak_start.store(start, Ordering::Relaxed);
        }
    }

    #[derive(Debug)]
    pub(crate) struct LocalQueue(UnsafeCell<VecDeque<Task>>);

    impl LocalQueue {
        pub(crate) fn new() -> Self {
            Self(UnsafeCell::new(VecDeque::new()))
        }

        pub(crate) fn push(&self, task: Task) {
            unsafe { (*self.0.get()).push_back(task) };
        }

        pub(crate) fn pop(&self) -> Option<Task> {
            unsafe { (*self.0.get()).pop_front() }
        }

        pub(crate) fn is_empty(&self) -> bool {
            unsafe { (*self.0.get()).is_empty() }
        }
    }

    #[derive(Debug)]
    pub(crate) struct RemoteQueue {
        fixed: Queue<ArrayBuffer<RemoteTask, REMOTE_BATCH>>,
        overflow: SegQueue<RemoteTask>,
    }

    impl RemoteQueue {
        pub(crate) fn new() -> Self {
            Self {
                fixed: Queue::default(),
                overflow: SegQueue::new(),
            }
        }

        pub(crate) fn push(&self, task: RemoteTask) {
            let result = self.fixed.try_enqueue([task]);
            if let Err(TryEnqueueError::InsufficientCapacity([task])) = result {
                self.overflow.push(task);
            }
        }

        /// Runs queued tasks, returns `true` if tasks are left.
        pub(crate) fn drain(&self, limit: usize, mut f: impl FnMut(RemoteTask)) -> bool {
            let fixed = match self.fixed.try_dequeue() {
                Ok(buf) => {
                    for task in buf {
                        f(task);
                    }
                    false
                }
                Err(TryDequeueError::Empty | TryDequeueError::Closed) => false,
                Err(_) => true,
            };
            for _ in 0..limit {
                if let Some(task) = self.overflow.pop() {
                    f(task);
                } else {
                    break;
                }
            }
            fixed || !self.overflow.is_empty()
        }

        pub(crate) fn clear(&self) {
            while self.overflow.pop().is_some() {}
            while self.fixed.try_dequeue().is_ok() {}
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
    /// streak is reported once. A count of 0 disables the warning.
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

#[cfg(all(test, ntex_rt_task))]
mod raw_tests;

#[cfg(all(test, ntex_rt_task, ntex_loom))]
mod raw_loom;

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::sync::atomic::AtomicUsize;
    use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
    use std::{cell::Cell, cell::RefCell, future::poll_fn, rc::Rc};

    use super::*;

    /// Iterations of a stress test, fewer under Miri.
    const fn iters(n: usize, miri: usize) -> usize {
        if cfg!(miri) { miri } else { n }
    }

    /// Wall clock limit of a wait, Miri runs much slower.
    const WAIT: Duration = Duration::from_secs(if cfg!(miri) { 600 } else { 30 });

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

    #[derive(Debug, Default)]
    struct CountNotify(Arc<std::sync::atomic::AtomicUsize>);

    impl Notify for CountNotify {
        fn notify(&self) -> io::Result<()> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        }
    }

    #[test]
    fn awake_skips_local_notify() {
        use std::sync::atomic::Ordering;

        let cnt = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let rt = Runtime::new(Box::new(CountNotify(cnt.clone())));

        // the driver is awake and polls the runtime before blocking
        rt.awake();
        rt.spawn(async {}).detach();
        assert_eq!(cnt.load(Ordering::Relaxed), 0);
        assert_eq!(rt.poll(), PollResult::Pending);

        // the runtime is idle, the driver may block
        rt.spawn(async {}).detach();
        rt.spawn(async {}).detach();
        assert_eq!(cnt.load(Ordering::Relaxed), 1);
        assert_eq!(rt.poll(), PollResult::Pending);
    }

    #[test]
    fn remote_schedule_notifies_once() {
        let cnt = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let rt = Runtime::new(Box::new(CountNotify(cnt.clone())));
        let spawn = |n| {
            let hnd = rt.handle();
            std::thread::spawn(move || {
                for _ in 0..n {
                    hnd.spawn(async {}).detach();
                }
            })
            .join()
            .unwrap();
        };

        spawn(100);
        assert_eq!(cnt.load(Ordering::Relaxed), 1);
        assert_eq!(rt.poll(), PollResult::Pending);

        // `poll` clears the flag
        spawn(100);
        assert_eq!(cnt.load(Ordering::Relaxed), 2);
        assert_eq!(rt.poll(), PollResult::Pending);
    }

    #[test]
    fn remote_schedule_no_lost_wakeup() {
        use std::sync::{Condvar, Mutex, atomic::AtomicUsize};
        use std::time::Duration;

        #[derive(Debug, Default)]
        struct Event(Mutex<bool>, Condvar);

        #[derive(Debug)]
        struct EventNotify(Arc<Event>);

        impl Notify for EventNotify {
            fn notify(&self) -> io::Result<()> {
                *self.0.0.lock().unwrap() = true;
                self.0.1.notify_one();
                Ok(())
            }
        }

        const THREADS: usize = 4;
        const TASKS: usize = iters(10_000, 50);
        const LOST: Duration = if cfg!(miri) { WAIT } else { Duration::from_secs(5) };

        let ev = Arc::new(Event::default());
        let rt = Runtime::new(Box::new(EventNotify(ev.clone())));
        let cnt = Arc::new(AtomicUsize::new(0));
        let threads: Vec<_> = (0..THREADS)
            .map(|_| {
                let (hnd, cnt) = (rt.handle(), cnt.clone());
                std::thread::spawn(move || {
                    for _ in 0..TASKS {
                        let cnt = cnt.clone();
                        hnd.spawn(async move {
                            cnt.fetch_add(1, Ordering::Relaxed);
                        })
                        .detach();
                    }
                })
            })
            .collect();

        // blocks like a driver, a lost wakeup times out
        while cnt.load(Ordering::Relaxed) < THREADS * TASKS {
            if rt.poll() == PollResult::Pending {
                let guard = ev.0.lock().unwrap();
                // the last tasks may have run in this poll
                let (mut guard, res) =
                    ev.1.wait_timeout_while(guard, LOST, |n| {
                        !*n && cnt.load(Ordering::Relaxed) < THREADS * TASKS
                    })
                    .unwrap();
                assert!(!res.timed_out(), "lost wakeup");
                *guard = false;
            }
        }
        for t in threads {
            t.join().unwrap();
        }
    }

    #[test]
    fn local_task_woken_by_remote_task() {
        use std::sync::{Mutex, atomic::AtomicBool, atomic::Ordering};

        let rt = Runtime::new(Box::new(NoopNotify));
        let waker = Arc::new(Mutex::new(None::<Waker>));
        let done = Rc::new(AtomicBool::new(false));
        let (waker2, done2) = (waker.clone(), done.clone());
        rt.spawn(poll_fn(move |cx| {
            if waker2.lock().unwrap().replace(cx.waker().clone()).is_some() {
                done2.store(true, Ordering::Relaxed);
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }))
        .detach();
        assert_eq!(rt.poll(), PollResult::Pending);

        let hnd = rt.handle();
        std::thread::spawn(move || {
            hnd.spawn(async move {
                waker.lock().unwrap().clone().unwrap().wake();
            })
            .detach();
        })
        .join()
        .unwrap();

        // the remote task schedules the local task, it must be polled again
        assert_eq!(rt.poll(), PollResult::PollAgain);
        assert_eq!(rt.poll(), PollResult::Pending);
        assert!(done.load(Ordering::Relaxed));
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
    fn now() {
        let rt = Runtime::new(Box::new(NoopNotify));

        // idle runtime returns the current time
        let t = Instant::now();
        assert!(rt.now() >= t);
        std::thread::sleep(Duration::from_millis(5));
        assert!(rt.now() >= t + Duration::from_millis(5));

        // tasks observe the turn start
        rt.awake();
        let start = rt.now();
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(rt.now(), start);

        let seen = Rc::new(RefCell::new(Vec::new()));
        for _ in 0..2 {
            let seen = seen.clone();
            rt.spawn(async move {
                seen.borrow_mut().push(Runtime::with_current(Runtime::now));
                std::thread::sleep(Duration::from_millis(5));
            })
            .detach();
        }
        CURRENT_RUNTIME.set(&rt, || assert_eq!(rt.poll(), PollResult::Pending));
        assert_eq!(*seen.borrow(), vec![start, start]);

        // idle again after the queue is drained
        assert!(rt.now() > start);
    }

    #[test]
    fn local_schedule_while_idle_starts_turn() {
        let sleep = Duration::from_millis(20);
        let rt = Runtime::new(Box::new(NoopNotify));
        rt.awake();
        let start = rt.now();
        std::thread::sleep(sleep);
        assert_eq!(rt.poll(), PollResult::Pending);
        let busy = rt.busy_duration();

        // the driver wakes a task before blocking, the next turn starts here
        rt.spawn(async {}).detach();
        rt.awake();
        assert!(rt.now() >= start + sleep);
        assert_eq!(rt.poll(), PollResult::Pending);
        assert!(rt.busy_duration() < busy + sleep);
    }

    #[test]
    fn busy_duration() {
        let sleep = Duration::from_millis(20);
        let rt = Runtime::builder()
            .event_interval(1)
            .build(Box::new(NoopNotify));
        let hnd = rt.handle();
        assert_eq!(rt.poll(), PollResult::Pending);

        // task time is counted
        rt.spawn(async move { std::thread::sleep(sleep) }).detach();
        assert_eq!(rt.poll(), PollResult::Pending);
        let busy = hnd.busy_duration().unwrap();
        assert!(busy >= sleep);

        // time between `awake` and `poll` is counted
        rt.awake();
        std::thread::sleep(sleep);
        assert_eq!(rt.poll(), PollResult::Pending);
        assert!(hnd.busy_duration().unwrap() >= busy + sleep);

        // idle time is not counted
        let busy = rt.busy_duration();
        std::thread::sleep(sleep);
        assert_eq!(rt.poll(), PollResult::Pending);
        assert!(rt.busy_duration() < busy + sleep);

        // published on each poll while the runtime stays busy
        rt.spawn(async move { std::thread::sleep(sleep) }).detach();
        rt.spawn(async {}).detach();
        let busy = rt.busy_duration();
        assert_eq!(rt.poll(), PollResult::PollAgain);
        let busy2 = rt.busy_duration();
        assert!(busy2 >= busy + sleep);

        // time between busy polls is counted
        std::thread::sleep(sleep);
        assert_eq!(rt.poll(), PollResult::Pending);
        assert!(rt.busy_duration() >= busy2 + sleep);
    }

    #[test]
    fn busy_duration_during_turn() {
        let sleep = Duration::from_millis(50);
        let (tx, rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let th = thread::spawn(move || {
            let rt = Runtime::new(Box::new(NoopNotify));
            let hnd = rt.handle();
            rt.spawn(async move {
                tx.send(hnd).unwrap();
                thread::sleep(sleep);
                // keep the turn running until the other thread checked it
                let _ = done_rx.recv();
            })
            .detach();
            assert_eq!(rt.poll(), PollResult::Pending);
            assert_eq!(rt.current_turn(), None);
        });

        // the task is running, its turn is visible from another thread
        let hnd = rx.recv().unwrap();
        thread::sleep(sleep / 2);
        assert!(hnd.busy_duration().unwrap() >= sleep / 2);
        assert!(hnd.current_turn().unwrap() >= sleep / 2);
        done_tx.send(()).unwrap();

        th.join().unwrap();
        assert!(hnd.busy_duration().unwrap() >= sleep);
        assert_eq!(hnd.current_turn(), None);
    }

    #[test]
    fn current_task() {
        let (tx, rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let (polled_tx, polled_rx) = std::sync::mpsc::channel::<()>();
        let (spawned_tx, spawned_rx) = std::sync::mpsc::channel::<()>();
        let th = thread::spawn(move || {
            let rt = Runtime::new(Box::new(NoopNotify));
            let hnd = rt.handle();
            assert_eq!(hnd.current_task(), None);

            let line = line!() + 1;
            rt.spawn(async move {
                tx.send((hnd, line)).unwrap();
                release_rx.recv().unwrap();
            })
            .detach();
            assert_eq!(rt.poll(), PollResult::Pending);
            polled_tx.send(()).unwrap();

            spawned_rx.recv().unwrap();
            assert_eq!(rt.poll(), PollResult::Pending);
        });

        // the running task is visible from another thread
        let (hnd, line) = rx.recv().unwrap();
        let task = hnd.current_task().unwrap();
        assert_eq!((task.file(), task.line()), (file!(), line));
        release_tx.send(()).unwrap();

        polled_rx.recv().unwrap();
        assert_eq!(hnd.current_task(), None);

        // tasks spawned from other threads record their location too
        let (tx, rx) = std::sync::mpsc::channel();
        let hnd2 = hnd.clone();
        let line = line!() + 1;
        hnd.spawn(async move {
            tx.send(hnd2.current_task()).unwrap();
        })
        .detach();
        spawned_tx.send(()).unwrap();
        th.join().unwrap();
        let task = rx.recv().unwrap().unwrap();
        assert_eq!((task.file(), task.line()), (file!(), line));
        assert_eq!(hnd.current_task(), None);
    }

    #[test]
    fn busy_duration_is_monotonic() {
        let done = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel();
        let done2 = done.clone();
        let th = thread::spawn(move || {
            let rt = Runtime::new(Box::new(NoopNotify));
            tx.send(rt.handle()).unwrap();
            for _ in 0..100 {
                rt.spawn(async { thread::sleep(Duration::from_micros(500)) })
                    .detach();
                assert_eq!(rt.poll(), PollResult::Pending);
                thread::sleep(Duration::from_micros(200));
            }
            done2.store(true, Ordering::Release);
        });

        // a busy period is not counted twice when it ends
        let hnd = rx.recv().unwrap();
        let mut last = Duration::ZERO;
        while !done.load(Ordering::Acquire) {
            let busy = hnd.busy_duration().unwrap();
            assert!(busy >= last, "{busy:?} < {last:?}");
            last = busy;
        }
        th.join().unwrap();
    }

    #[test]
    fn poll_count() {
        let rt = Runtime::builder()
            .event_interval(1)
            .build(Box::new(NoopNotify));
        let hnd = rt.handle();
        assert_eq!((rt.poll_count(), rt.poll_again_count()), (0, 0));

        assert_eq!(rt.poll(), PollResult::Pending);
        assert_eq!((rt.poll_count(), rt.poll_again_count()), (1, 0));

        // one task per poll, the first poll leaves a backlog
        rt.spawn(async {}).detach();
        rt.spawn(async {}).detach();
        assert_eq!(rt.poll(), PollResult::PollAgain);
        assert_eq!(rt.poll(), PollResult::Pending);
        assert_eq!(hnd.poll_count(), Some(3));
        assert_eq!(hnd.poll_again_count(), Some(1));
        assert_eq!(hnd.poll_histogram().unwrap().count(), 3);

        // a stopped runtime does not run tasks
        rt.stop.set(true);
        assert_eq!(rt.poll(), PollResult::Ready);
        assert_eq!((rt.poll_count(), rt.poll_again_count()), (3, 1));
        assert_eq!(rt.poll_histogram().count(), 3);
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

    #[test]
    fn self_wake() {
        let rt = Runtime::new(Box::new(NoopNotify));
        let waker = Rc::new(RefCell::new(None::<Waker>));
        let waker2 = waker.clone();
        let mut polls = 0;
        let h = rt.spawn(poll_fn(move |cx| {
            polls += 1;
            match polls {
                1..=5 => {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
                6 => {
                    *waker2.borrow_mut() = Some(cx.waker().clone());
                    Poll::Pending
                }
                _ => Poll::Ready(()),
            }
        }));
        CURRENT_RUNTIME.set(&rt, || while rt.poll() == PollResult::PollAgain {});
        assert_eq!(rt.self_wake_count(), 5);
        assert_eq!(rt.handle().self_wake_count(), Some(5));

        // a wake from outside of the poll is not a self-wake
        waker.borrow_mut().take().unwrap().wake();
        CURRENT_RUNTIME.set(&rt, || while rt.poll() == PollResult::PollAgain {});
        assert!(h.is_finished());
        assert_eq!(rt.self_wake_count(), 5);
    }

    #[test]
    fn self_wake_streak() {
        #[derive(Default)]
        struct Stats {
            wakes: Cell<u32>,
            start: Cell<u32>,
        }

        impl TaskStats for &Stats {
            fn location(self) -> SpawnLocation {
                Location::caller()
            }
            fn self_wakes(self) -> u32 {
                self.wakes.get()
            }
            fn set_self_wakes(self, wakes: u32) {
                self.wakes.set(wakes);
            }
            fn streak_start(self) -> u32 {
                self.start.get()
            }
            fn set_streak_start(self, start: u32) {
                self.start.set(start);
            }
        }

        fn streak(rt: &Runtime, n: u32) -> u32 {
            let stats = Stats::default();
            for _ in 0..n {
                rt.queue.self_wake(&stats, stats.wakes.get());
            }
            stats.wakes.get()
        }

        let rt = Runtime::builder()
            .self_wake_threshold(3, Duration::ZERO)
            .build(Box::new(NoopNotify));
        assert_eq!(streak(&rt, 2), 2);
        assert_eq!(streak(&rt, 3), 3 | REPORTED);
        assert_eq!(streak(&rt, 5), 5 | REPORTED);
        assert_eq!(rt.self_wake_count(), 10);

        // the streak is shorter than the duration
        let rt = Runtime::builder()
            .self_wake_threshold(3, Duration::from_hours(1))
            .build(Box::new(NoopNotify));
        assert_eq!(streak(&rt, 5), 5);

        // disabled
        let rt = Runtime::builder()
            .self_wake_threshold(0, Duration::ZERO)
            .build(Box::new(NoopNotify));
        assert_eq!(streak(&rt, 5), 5);
    }

    struct DropFlag(Arc<AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    type Slot = Arc<std::sync::Mutex<Option<Waker>>>;

    fn poll_until(rt: &Runtime, cond: impl Fn() -> bool) {
        let start = Instant::now();
        CURRENT_RUNTIME.set(rt, || {
            while !cond() {
                assert!(start.elapsed() < WAIT, "timeout");
                if rt.poll() == PollResult::Pending {
                    thread::yield_now();
                }
            }
        });
    }

    fn block_on_thread<F: Future>(fut: F) -> F::Output {
        struct ThreadWaker(thread::Thread);

        impl std::task::Wake for ThreadWaker {
            fn wake(self: Arc<Self>) {
                self.0.unpark();
            }
        }

        let waker = Waker::from(Arc::new(ThreadWaker(thread::current())));
        let mut cx = std::task::Context::from_waker(&waker);
        let mut fut = std::pin::pin!(fut);
        loop {
            if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
                return v;
            }
            thread::park();
        }
    }

    /// Stores its waker until polled `n` times.
    fn wait_polls(
        slot: Slot,
        polls: Arc<std::sync::atomic::AtomicUsize>,
        n: usize,
    ) -> impl Future<Output = usize> {
        poll_fn(move |cx| {
            let cnt = polls.fetch_add(1, Ordering::SeqCst) + 1;
            if cnt >= n {
                Poll::Ready(cnt)
            } else {
                *slot.lock().unwrap() = Some(cx.waker().clone());
                Poll::Pending
            }
        })
    }

    #[test]
    fn remote_wakers() {
        let rt = Runtime::new(Box::new(NoopNotify));
        let slot = Slot::default();
        let polls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let h = rt.spawn(wait_polls(slot.clone(), polls.clone(), 4));
        poll_until(&rt, || polls.load(Ordering::SeqCst) == 1);

        // wake by ref, the thread keeps the waker
        let w = slot.lock().unwrap().take().unwrap();
        let th = thread::spawn(move || {
            w.wake_by_ref();
            w
        });
        let w = th.join().unwrap();
        poll_until(&rt, || polls.load(Ordering::SeqCst) == 2);

        // clone, drop and wake by value
        let w2 = slot.lock().unwrap().take().unwrap();
        thread::spawn(move || {
            let c = w2.clone();
            drop(w2);
            drop(w);
            c.wake();
        })
        .join()
        .unwrap();
        poll_until(&rt, || polls.load(Ordering::SeqCst) == 3);

        // joined on another thread while the runtime polls
        let w = slot.lock().unwrap().take().unwrap();
        let th = thread::spawn(move || {
            w.wake();
            block_on_thread(h)
        });
        poll_until(&rt, || th.is_finished());
        assert_eq!(th.join().unwrap().unwrap(), 4);
    }

    #[test]
    fn remote_wakers_stress() {
        const POLLS: usize = iters(5000, 50);

        let rt = Runtime::new(Box::new(NoopNotify));
        let slot = Slot::default();
        let polls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let h = rt.spawn(wait_polls(slot.clone(), polls.clone(), POLLS));

        let wakers: Vec<_> = (0..4)
            .map(|i| {
                let slot = slot.clone();
                let stop = stop.clone();
                thread::spawn(move || {
                    while !stop.load(Ordering::SeqCst) {
                        let w = slot.lock().unwrap().clone();
                        match (w, i % 3) {
                            (Some(w), 0) => w.wake(),
                            (Some(w), 1) => w.wake_by_ref(),
                            (Some(w), _) => drop(w.clone()),
                            (None, _) => thread::yield_now(),
                        }
                    }
                })
            })
            .collect();
        let joiner = thread::spawn(move || block_on_thread(h));

        poll_until(&rt, || joiner.is_finished());
        stop.store(true, Ordering::SeqCst);
        for th in wakers {
            th.join().unwrap();
        }
        assert_eq!(joiner.join().unwrap().unwrap(), POLLS);
        assert_eq!(polls.load(Ordering::SeqCst), POLLS);
        poll_until(&rt, || true);
    }

    #[test]
    fn last_waker_drops_future() {
        let rt = Runtime::new(Box::new(NoopNotify));
        for remote in [false, true] {
            let slot = Slot::default();
            let dropped = Arc::new(AtomicBool::new(false));
            let flag = DropFlag(dropped.clone());
            let slot2 = slot.clone();
            rt.spawn(async move {
                let _flag = flag;
                poll_fn(|cx| {
                    *slot2.lock().unwrap() = Some(cx.waker().clone());
                    Poll::<()>::Pending
                })
                .await;
            })
            .detach();
            poll_until(&rt, || slot.lock().unwrap().is_some());
            assert!(!dropped.load(Ordering::SeqCst));

            let w = slot.lock().unwrap().take();
            if remote {
                thread::spawn(move || drop(w)).join().unwrap();
            } else {
                drop(w);
            }
            poll_until(&rt, || dropped.load(Ordering::SeqCst));
        }
    }

    #[test]
    fn remote_cancel() {
        let rt = Runtime::new(Box::new(NoopNotify));
        let dropped = Arc::new(AtomicBool::new(false));
        let flag = DropFlag(dropped.clone());
        let polled = Rc::new(Cell::new(false));
        let polled2 = polled.clone();
        let h = rt.spawn(async move {
            let _flag = flag;
            polled2.set(true);
            poll_fn(|_| Poll::<()>::Pending).await;
        });
        poll_until(&rt, || polled.get());

        thread::spawn(move || h.cancel()).join().unwrap();
        poll_until(&rt, || dropped.load(Ordering::SeqCst));
    }

    #[test]
    fn cancel_while_running() {
        let rt = Runtime::new(Box::new(NoopNotify));
        let dropped = Arc::new(AtomicBool::new(false));
        let flag = DropFlag(dropped.clone());
        let handle = Rc::new(RefCell::new(None::<JoinHandle<()>>));
        let handle2 = handle.clone();
        let polls = Rc::new(Cell::new(0));
        let polls2 = polls.clone();
        let h = rt.spawn(async move {
            let _flag = flag;
            poll_fn(|cx| {
                polls2.set(polls2.get() + 1);
                if let Some(h) = handle2.borrow_mut().take() {
                    cx.waker().wake_by_ref();
                    h.cancel();
                }
                Poll::<()>::Pending
            })
            .await;
        });
        *handle.borrow_mut() = Some(h);
        poll_until(&rt, || dropped.load(Ordering::SeqCst));
        assert_eq!(polls.get(), 1);
    }

    #[test]
    fn panic_in_poll() {
        let rt = Runtime::new(Box::new(NoopNotify));
        let dropped = Arc::new(AtomicBool::new(false));
        let flag = DropFlag(dropped.clone());
        let h = rt.spawn(async move {
            let _flag = flag;
            panic!("task panic");
        });
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            CURRENT_RUNTIME.set(&rt, || rt.poll())
        }));
        assert!(res.is_err());
        assert!(dropped.load(Ordering::SeqCst));
        assert!(h.is_finished());
        #[cfg(ntex_rt_task)]
        assert!(block_on_thread(h).is_err());

        // the runtime keeps running tasks
        let done = Rc::new(Cell::new(false));
        let done2 = done.clone();
        rt.spawn(async move { done2.set(true) }).detach();
        poll_until(&rt, || done.get());
    }

    #[test]
    fn drop_runtime_with_tasks() {
        let dropped = Arc::new(AtomicBool::new(false));
        let remote = Arc::new(AtomicBool::new(false));
        let rt = Runtime::new(Box::new(NoopNotify));
        let flag = DropFlag(dropped.clone());
        let h = rt.spawn(async move {
            let _flag = flag;
            poll_fn(|_| Poll::<()>::Pending).await;
        });
        let flag = DropFlag(remote.clone());
        let hnd = rt.handle();
        thread::spawn(move || {
            hnd.spawn(async move {
                let _flag = flag;
            })
            .detach();
        })
        .join()
        .unwrap();
        drop(rt);
        assert!(dropped.load(Ordering::SeqCst));
        assert!(remote.load(Ordering::SeqCst));
        assert!(h.is_finished());
    }

    /// Pending future that borrows the caller's frame and sets a flag on drop.
    struct Borrowing<'a>(&'a Cell<bool>, Option<Waker>);

    impl Future for Borrowing<'_> {
        type Output = ();

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            assert!(!self.0.get());
            self.1 = Some(cx.waker().clone());
            Poll::Pending
        }
    }

    impl Drop for Borrowing<'_> {
        fn drop(&mut self) {
            self.0.set(true);
        }
    }

    /// A task that polls the runtime and wakes itself is not run by a notify
    /// that reenters the runtime, the running poll picks it up.
    #[test]
    fn reentrant_notify_self_wake() {
        #[derive(Debug, Default)]
        struct ReenterNotify(AtomicUsize);

        impl Notify for ReenterNotify {
            fn notify(&self) -> io::Result<()> {
                if CURRENT_RUNTIME.is_set() {
                    CURRENT_RUNTIME.with(|rt| {
                        if !rt.queue.current.load(Ordering::Relaxed).is_null() {
                            self.0.fetch_add(1, Ordering::Relaxed);
                        }
                        rt.poll();
                    });
                }
                Ok(())
            }
        }

        #[derive(Debug)]
        struct Shared(Arc<ReenterNotify>);

        impl Notify for Shared {
            fn notify(&self) -> io::Result<()> {
                self.0.notify()
            }
        }

        let notify = Arc::new(ReenterNotify::default());
        let rt = Runtime::new(Box::new(Shared(notify.clone())));
        let polls = Rc::new(Cell::new(0));
        let p = polls.clone();
        rt.spawn_detached(poll_fn(move |cx| {
            p.set(p.get() + 1);
            if p.get() == 1 {
                CURRENT_RUNTIME.with(|rt| {
                    rt.poll();
                });
                cx.waker().wake_by_ref();
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        }));
        CURRENT_RUNTIME.set(&rt, || {
            rt.poll();
            rt.poll();
        });
        assert_eq!(polls.get(), 2);
        assert_eq!(notify.0.load(Ordering::Relaxed), 0);
        assert_eq!(rt.self_wake_count(), 1);
        assert!(rt.queue.current.load(Ordering::Relaxed).is_null());
    }

    #[test]
    fn block_on_rejects_nesting() {
        struct BusyDriver;

        impl Driver for BusyDriver {
            fn handle(&self) -> Box<dyn Notify> {
                Box::new(NoopNotify)
            }

            fn run(&self, rt: &Runtime) -> io::Result<()> {
                while rt.poll() != PollResult::Ready {}
                Ok(())
            }
        }

        for same in [true, false] {
            let rt = Runtime::new(Box::new(NoopNotify));
            let nested = rt.block_on(
                async {
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        if same {
                            Runtime::with_current(|rt| rt.block_on(async {}, &BusyDriver));
                        } else {
                            Runtime::new(Box::new(NoopNotify)).block_on(async {}, &BusyDriver);
                        }
                    }))
                    .is_err()
                },
                &BusyDriver,
            );
            assert!(nested);
            // the runtime is usable after the rejected call
            assert_eq!(rt.block_on(async { 1 }, &BusyDriver), 1);
        }
    }

    #[test]
    fn block_on_drops_future_on_driver_error() {
        struct ErrorDriver;

        impl Driver for ErrorDriver {
            fn handle(&self) -> Box<dyn Notify> {
                Box::new(NoopNotify)
            }

            fn run(&self, _: &Runtime) -> io::Result<()> {
                Err(io::Error::other("driver stopped"))
            }
        }

        let rt = Runtime::new(Box::new(NoopNotify));
        let dropped = Cell::new(false);
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            rt.block_on(Borrowing(&dropped, None), &ErrorDriver);
        }));
        assert!(res.is_err());
        assert!(dropped.get());
        rt.poll();
    }

    #[test]
    fn block_on_drops_future_on_driver_panic() {
        struct PanicDriver;

        impl Driver for PanicDriver {
            fn handle(&self) -> Box<dyn Notify> {
                Box::new(NoopNotify)
            }

            fn run(&self, rt: &Runtime) -> io::Result<()> {
                rt.poll();
                panic!("driver panic");
            }
        }

        let rt = Runtime::new(Box::new(NoopNotify));
        let dropped = Cell::new(false);
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            rt.block_on(Borrowing(&dropped, None), &PanicDriver);
        }));
        assert!(res.is_err());
        assert!(dropped.get());
        rt.poll();
    }

    #[derive(Debug, Default, Clone)]
    struct Counter(Arc<AtomicUsize>);

    impl Counter {
        fn get(&self) -> usize {
            self.0.load(Ordering::SeqCst)
        }

        fn inc(&self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }

        fn token(&self) -> Token {
            Token(self.clone())
        }
    }

    /// Increments the counter on drop.
    #[derive(Debug)]
    struct Token(Counter);

    impl Drop for Token {
        fn drop(&mut self) {
            self.0.inc();
        }
    }

    #[derive(Default)]
    struct CountWaker(AtomicUsize);

    impl std::task::Wake for CountWaker {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn drain(rt: &Runtime) {
        CURRENT_RUNTIME.set(rt, || while rt.poll() == PollResult::PollAgain {});
    }

    async fn yield_once() {
        let mut yielded = false;
        poll_fn(|cx| {
            if yielded {
                Poll::Ready(())
            } else {
                yielded = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        })
        .await;
    }

    #[test]
    fn remote_join_race() {
        const N: usize = iters(2000, 50);

        let rt = Runtime::new(Box::new(NoopNotify));
        let (tx, rx) = std::sync::mpsc::channel::<JoinHandle<usize>>();
        let done = Counter::default();
        let done2 = done.clone();
        let th = thread::spawn(move || {
            for (i, h) in rx.into_iter().enumerate() {
                assert_eq!(block_on_thread(h).unwrap(), i);
                done2.inc();
            }
        });
        for i in 0..N {
            // completes while the joining thread registers its waker
            let h = rt.spawn(async move {
                for _ in 0..i % 4 {
                    yield_once().await;
                }
                i
            });
            tx.send(h).unwrap();
            poll_until(&rt, || done.get() == i + 1);
        }
        drop(tx);
        th.join().unwrap();
    }

    #[test]
    fn unconsumed_output_dropped() {
        let rt = Runtime::new(Box::new(NoopNotify));
        for remote in [false, true] {
            let out = Counter::default();
            let tok = out.token();
            let h = rt.spawn(async move { tok });
            poll_until(&rt, || h.is_finished());
            assert_eq!(out.get(), 0);
            if remote {
                thread::spawn(move || drop(h)).join().unwrap();
            } else {
                drop(h);
            }
            assert_eq!(out.get(), 1);
        }

        // detached before completion
        let out = Counter::default();
        let tok = out.token();
        rt.spawn(async move {
            yield_once().await;
            tok
        })
        .detach();
        poll_until(&rt, || out.get() == 1);

        // consumed by the join handle
        let out = Counter::default();
        let tok = out.token();
        let h = rt.spawn(async move { tok });
        poll_until(&rt, || h.is_finished());
        drop(block_on_thread(h).unwrap());
        drain(&rt);
        assert_eq!(out.get(), 1);
    }

    #[test]
    fn remote_cancel_before_run() {
        let rt = Runtime::new(Box::new(NoopNotify));
        let dropped = Arc::new(AtomicBool::new(false));
        let polled = Arc::new(AtomicBool::new(false));
        let hnd = rt.handle();
        let flag = DropFlag(dropped.clone());
        let polled2 = polled.clone();
        thread::spawn(move || {
            hnd.spawn(async move {
                let _flag = flag;
                polled2.store(true, Ordering::SeqCst);
            })
            .cancel();
        })
        .join()
        .unwrap();
        poll_until(&rt, || dropped.load(Ordering::SeqCst));
        assert!(!polled.load(Ordering::SeqCst));
    }

    #[test]
    fn cancel_complete_race() {
        let rt = Runtime::new(Box::new(NoopNotify));
        let created = Counter::default();
        let out = Counter::default();
        for _ in 0..iters(500, 10) {
            let dropped = Arc::new(AtomicBool::new(false));
            let flag = DropFlag(dropped.clone());
            let slot = Slot::default();
            let (slot2, created2, out2) = (slot.clone(), created.clone(), out.clone());
            let h = rt.spawn(async move {
                let _flag = flag;
                let mut first = true;
                poll_fn(|cx| {
                    if first {
                        first = false;
                        *slot2.lock().unwrap() = Some(cx.waker().clone());
                        Poll::Pending
                    } else {
                        Poll::Ready(())
                    }
                })
                .await;
                created2.inc();
                out2.token()
            });
            poll_until(&rt, || slot.lock().unwrap().is_some());

            let w = slot.lock().unwrap().take().unwrap();
            let t1 = thread::spawn(move || w.wake());
            let t2 = thread::spawn(move || h.cancel());
            poll_until(&rt, || t1.is_finished() && t2.is_finished());
            t1.join().unwrap();
            t2.join().unwrap();
            // the output is dropped once, whichever side wins
            poll_until(&rt, || {
                dropped.load(Ordering::SeqCst) && created.get() == out.get()
            });
        }
    }

    #[test]
    fn stale_wakers() {
        let rt = Runtime::new(Box::new(NoopNotify));
        let slot = Slot::default();
        let polls = Arc::new(AtomicUsize::new(0));
        let out = Counter::default();
        let tok = out.token();
        let (slot2, polls2) = (slot.clone(), polls.clone());
        let h = rt.spawn(async move {
            wait_polls(slot2, polls2, 2).await;
            tok
        });
        poll_until(&rt, || polls.load(Ordering::SeqCst) == 1);

        let w = slot.lock().unwrap().take().unwrap();
        let (w2, w3) = (w.clone(), w.clone());
        w.wake();
        poll_until(&rt, || h.is_finished());

        // wakes after completion do not run the task
        w2.wake_by_ref();
        let w3 = thread::spawn(move || {
            w3.wake_by_ref();
            w3
        })
        .join()
        .unwrap();
        drain(&rt);
        assert_eq!(polls.load(Ordering::SeqCst), 2);

        // the output is dropped with the join handle
        drop(h);
        assert_eq!(out.get(), 1);
        // the last waker, dropped on another thread, frees the task
        drop(w2);
        thread::spawn(move || w3.wake()).join().unwrap();
        drain(&rt);
    }

    #[test]
    fn unclaimed_output_with_own_waker() {
        // the output holds a waker of its task, a cycle unless it is dropped
        for case in 0..5 {
            let rt = Runtime::new(Box::new(NoopNotify));
            let dropped = Arc::new(AtomicBool::new(false));
            let mut flag = Some(DropFlag(dropped.clone()));
            let h = rt.spawn(poll_fn(move |cx| {
                Poll::Ready((cx.waker().clone(), flag.take().unwrap()))
            }));
            let remote_drop = |h| thread::spawn(move || drop(h)).join().unwrap();
            match case {
                0 => h.detach(),
                1 => remote_drop(h),
                2 => {
                    drain(&rt);
                    h.detach();
                }
                3 => {
                    drain(&rt);
                    h.cancel();
                }
                _ => {
                    drain(&rt);
                    remote_drop(h);
                }
            }
            drain(&rt);
            assert!(dropped.load(Ordering::SeqCst), "case {case}");
        }
    }

    struct ReDrop {
        waker: Rc<RefCell<Option<Waker>>>,
        other: Option<JoinHandle<()>>,
        hnd: Handle,
        spawned: Arc<AtomicBool>,
        dropped: Arc<AtomicBool>,
    }

    impl Drop for ReDrop {
        fn drop(&mut self) {
            // wakes itself, drops a handle and spawns a task
            if let Some(w) = self.waker.borrow_mut().take() {
                w.wake();
            }
            drop(self.other.take());
            let flag = DropFlag(self.spawned.clone());
            self.hnd
                .spawn(async move {
                    let _flag = flag;
                })
                .detach();
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn reentrant_drop() {
        for clear in [false, true] {
            let rt = Runtime::new(Box::new(NoopNotify));
            let other_dropped = Arc::new(AtomicBool::new(false));
            let spawned = Arc::new(AtomicBool::new(false));
            let dropped = Arc::new(AtomicBool::new(false));
            let flag = DropFlag(other_dropped.clone());
            let other = rt.spawn(async move {
                let _flag = flag;
                poll_fn(|_| Poll::<()>::Pending).await;
            });
            let waker = Rc::new(RefCell::new(None::<Waker>));
            let redrop = ReDrop {
                waker: waker.clone(),
                other: Some(other),
                hnd: rt.handle(),
                spawned: spawned.clone(),
                dropped: dropped.clone(),
            };
            let waker2 = waker.clone();
            let h = rt.spawn(async move {
                let _redrop = redrop;
                poll_fn(|cx| {
                    *waker2.borrow_mut() = Some(cx.waker().clone());
                    Poll::<()>::Pending
                })
                .await;
            });
            poll_until(&rt, || waker.borrow().is_some());

            if clear {
                // queued when the runtime is dropped
                h.detach();
                waker.borrow().as_ref().unwrap().wake_by_ref();
                drop(rt);
            } else {
                h.cancel();
                poll_until(&rt, || spawned.load(Ordering::SeqCst));
            }
            assert!(dropped.load(Ordering::SeqCst));
            assert!(other_dropped.load(Ordering::SeqCst));
            assert!(spawned.load(Ordering::SeqCst));
        }
    }

    /// Wakes its task and polls the runtime on drop.
    struct PollOnDrop {
        waker: Rc<RefCell<Option<Waker>>>,
        drops: Counter,
    }

    impl Drop for PollOnDrop {
        fn drop(&mut self) {
            self.drops.inc();
            let waker = self.waker.borrow_mut().take();
            if let Some(w) = waker {
                w.wake();
            }
            Runtime::with_current(|rt| while rt.poll() == PollResult::PollAgain {});
        }
    }

    #[test]
    fn reentrant_poll_in_drop() {
        for clear in [false, true] {
            let rt = Runtime::new(Box::new(NoopNotify));
            let drops = Counter::default();
            let polls = Counter::default();
            let waker = Rc::new(RefCell::new(None::<Waker>));
            let guard = PollOnDrop {
                waker: waker.clone(),
                drops: drops.clone(),
            };
            let (waker2, polls2) = (waker.clone(), polls.clone());
            let h = rt.spawn(async move {
                let _guard = guard;
                poll_fn(|cx| {
                    polls2.inc();
                    *waker2.borrow_mut() = Some(cx.waker().clone());
                    Poll::<()>::Pending
                })
                .await;
            });
            poll_until(&rt, || waker.borrow().is_some());

            if clear {
                // queued when the runtime is dropped
                h.detach();
                waker.borrow().as_ref().unwrap().wake_by_ref();
                drop(rt);
            } else {
                h.cancel();
                drain(&rt);
            }
            // the future is neither polled nor dropped again
            assert_eq!(drops.get(), 1);
            assert_eq!(polls.get(), 1);
        }
    }

    /// Waker that runs a callback when it is cloned or released.
    struct Hook {
        refs: Cell<usize>,
        on_clone: Cell<Option<Box<dyn FnOnce()>>>,
        on_release: Cell<Option<Box<dyn FnOnce()>>>,
    }

    impl Hook {
        const VTABLE: RawWakerVTable =
            RawWakerVTable::new(Self::clone, Self::release, |_| (), Self::release);

        fn waker(
            on_clone: Option<Box<dyn FnOnce()>>,
            on_release: Option<Box<dyn FnOnce()>>,
        ) -> Waker {
            let hook = Box::into_raw(Box::new(Hook {
                refs: Cell::new(1),
                on_clone: Cell::new(on_clone),
                on_release: Cell::new(on_release),
            }));
            unsafe { Waker::from_raw(RawWaker::new(hook.cast(), &Self::VTABLE)) }
        }

        unsafe fn clone(p: *const ()) -> RawWaker {
            let hook = unsafe { &*p.cast::<Hook>() };
            hook.refs.set(hook.refs.get() + 1);
            if let Some(f) = hook.on_clone.take() {
                f();
            }
            RawWaker::new(p, &Self::VTABLE)
        }

        unsafe fn release(p: *const ()) {
            let hook = unsafe { &*p.cast::<Hook>() };
            hook.refs.set(hook.refs.get() - 1);
            if hook.refs.get() == 0 {
                if let Some(f) = hook.on_release.take() {
                    f();
                }
                drop(unsafe { Box::from_raw(p.cast_mut().cast::<Hook>()) });
            }
        }
    }

    #[test]
    fn join_completes_in_waker_callback() {
        // the target task completes while the join handle replaces its waker
        for in_clone in [false, true] {
            let rt = Runtime::new(Box::new(NoopNotify));
            CURRENT_RUNTIME.set(&rt, || {
                let ready = Rc::new(Cell::new(false));
                let waker = Rc::new(RefCell::new(None::<Waker>));
                let (ready2, waker2) = (ready.clone(), waker.clone());
                let mut h = rt.spawn(poll_fn(move |cx| {
                    if ready2.get() {
                        Poll::Ready(1)
                    } else {
                        *waker2.borrow_mut() = Some(cx.waker().clone());
                        Poll::Pending
                    }
                }));
                while rt.poll() == PollResult::PollAgain {}

                let complete: Box<dyn FnOnce()> = Box::new(move || {
                    ready.set(true);
                    waker.borrow_mut().take().unwrap().wake();
                    Runtime::with_current(|rt| while rt.poll() == PollResult::PollAgain {});
                });
                let (on_clone, on_release) = if in_clone {
                    (Some(complete), None)
                } else {
                    (None, Some(complete))
                };

                let first = Hook::waker(None, on_release);
                let res = Pin::new(&mut h).poll(&mut Context::from_waker(&first));
                assert!(res.is_pending());
                // the join handle keeps the last reference
                drop(first);

                let second = Hook::waker(on_clone, None);
                let res = Pin::new(&mut h).poll(&mut Context::from_waker(&second));
                assert!(matches!(res, Poll::Ready(Ok(1))));
            });
        }
    }

    #[cfg(ntex_rt_task)]
    #[test]
    fn panic_in_future_drop() {
        struct PanicOnDrop(Counter);

        impl Drop for PanicOnDrop {
            fn drop(&mut self) {
                self.0.inc();
                panic!("drop panic");
            }
        }

        struct PanicFut {
            _panic: PanicOnDrop,
            _flag: DropFlag,
            ready: bool,
        }

        impl Future for PanicFut {
            type Output = usize;

            fn poll(self: Pin<&mut Self>, _: &mut std::task::Context<'_>) -> Poll<usize> {
                if self.ready { Poll::Ready(1) } else { Poll::Pending }
            }
        }

        let rt = Runtime::new(Box::new(NoopNotify));
        for ready in [true, false] {
            let drops = Counter::default();
            let dropped = Arc::new(AtomicBool::new(false));
            let h = rt.spawn(PanicFut {
                _panic: PanicOnDrop(drops.clone()),
                _flag: DropFlag(dropped.clone()),
                ready,
            });
            let h = if ready {
                h
            } else {
                drain(&rt);
                h.cancel();
                rt.spawn(PanicFut {
                    _panic: PanicOnDrop(drops.clone()),
                    _flag: DropFlag(dropped.clone()),
                    ready: true,
                })
            };
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drain(&rt)));
            assert!(res.is_err());
            assert!(dropped.load(Ordering::SeqCst));
            if !ready {
                // the second task panics on the next run
                let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drain(&rt)));
                assert!(res.is_err());
            }
            assert!(h.is_finished());
            assert!(block_on_thread(h).is_err());
            drain(&rt);
            assert_eq!(drops.get(), if ready { 1 } else { 2 });
        }
    }

    #[test]
    fn join_waker_replaced() {
        fn poll_with(h: &mut JoinHandle<usize>, w: &Arc<CountWaker>) -> Poll<usize> {
            let w = Waker::from(w.clone());
            let mut cx = std::task::Context::from_waker(&w);
            Pin::new(h).poll(&mut cx).map(Result::unwrap)
        }

        let rt = Runtime::new(Box::new(NoopNotify));
        for remote in [false, true] {
            let slot = Slot::default();
            let polls = Arc::new(AtomicUsize::new(0));
            let mut h = rt.spawn(wait_polls(slot.clone(), polls.clone(), 2));
            poll_until(&rt, || polls.load(Ordering::SeqCst) == 1);

            let a = Arc::new(CountWaker::default());
            let b = Arc::new(CountWaker::default());
            let (a2, b2) = (a.clone(), b.clone());
            let poll_both = move |mut h| {
                assert!(poll_with(&mut h, &a2).is_pending());
                assert!(poll_with(&mut h, &b2).is_pending());
                h
            };
            h = if remote {
                thread::spawn(move || poll_both(h)).join().unwrap()
            } else {
                poll_both(h)
            };

            slot.lock().unwrap().take().unwrap().wake();
            poll_until(&rt, || b.0.load(Ordering::SeqCst) == 1);
            assert_eq!(a.0.load(Ordering::SeqCst), 0);
            assert_eq!(poll_with(&mut h, &b), Poll::Ready(2));
        }
    }
}
