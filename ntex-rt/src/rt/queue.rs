//! Run queues of a runtime, shared by the runtime and the tasks.
use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use std::{borrow::Cow, cell::Cell, fmt, future::Future, io, panic::Location, ptr, sync::Arc};

#[cfg(not(ntex_loom))]
use std::{
    sync::atomic::{AtomicBool, AtomicUsize},
    thread::yield_now,
};

#[cfg(ntex_loom)]
use loom::{
    sync::atomic::{AtomicBool, AtomicUsize},
    thread::yield_now,
};

use crate::handle::{JoinHandle, SpawnLocation};
use crate::{busy::BusyClock, driver::Notify, metrics::AtomicHistogram};

use super::task::{self, LocalQueue, Remote as RemoteTask, Runnable as Task, TaskList};
use super::wake::{self, WakeEntry, WakeQueue};
use super::{LOG_TARGET, REPORTED, RuntimeBuilder, TaskStats, ThreadId, current_thread_id};

const MAX_SELF_WAKES: u32 = task::MAX_SELF_WAKES;

/// Remote commands run per turn, besides `event_interval` overflow commands.
const REMOTE_BATCH: usize = 128;

/// The remaining bits count remote publications admitted before shutdown.
const CLOSED: usize = 1 << (usize::BITS - 1);

#[repr(C)]
pub(crate) struct RunnableQueue {
    /// Immutable owner-thread identity, also read by remote scheduling code.
    pub(crate) id: ThreadId,
    /// Never-reused identity of this queue's registered wake endpoint.
    pub(crate) runtime_id: wake::RuntimeId,
    /// Driver notifier, callable from the owner and remote producers.
    pub(super) handle: Box<dyn Notify>,
    /// Epoch for per-task self-wake streak timestamps.
    pub(super) epoch: Instant,
    /// Minimum streak duration before a possible busy loop is reported.
    self_wake_time: Duration,
    /// Local task budget per turn; also contributes to remote drain budgets.
    event_interval: usize,
    /// Minimum self-wake count to report; zero disables per-task streak tracking.
    self_wake_limit: u32,
    /// Aligns producer-written state away from the read-only fields.
    _remote: [CacheLine; 0],
    /// Coalesces remote driver notifications until the next scheduler drain.
    notified: AtomicBool,
    /// Closed bit and count of admitted remote submissions still in flight.
    admission: AtomicUsize,
    /// Submission, reference and wake commands, processed in FIFO order by the owner.
    pub(crate) wake_queue: WakeQueue,
    /// Aligns owner-written scheduling state away from producer-written fields.
    _local: [CacheLine; 0],
    /// Idle state used for driver notifications and busy-time accounting.
    pub(super) idle: Cell<bool>,
    /// Start of the current runtime turn, meaningful while the runtime is busy.
    pub(super) poll_start: Cell<Instant>,
    /// Cached `poll_start` offset from the epoch, in wrapping milliseconds.
    pub(super) self_wake_clock: Cell<Option<u32>>,
    /// Shared wall-clock busy-time totals and current-turn timing.
    pub(super) busy: BusyClock,
    /// Non-stopped scheduler polls; written by the owner and read across threads.
    pub(super) polls: AtomicU64,
    /// Scheduler polls that left queued work; written by the owner.
    pub(super) polls_again: AtomicU64,
    /// Shared histogram of scheduler poll durations.
    pub(super) poll_histogram: AtomicHistogram,
    /// Total self-wakes observed during task polls; written by the owner.
    pub(super) self_wakes: AtomicU64,
    /// Spawn location of the running task, null between tasks.
    pub(super) current: AtomicPtr<Location<'static>>,
    /// Owner-only FIFO of runnables, each holding a task reference.
    pub(super) local_queue: LocalQueue,
    /// Non-owning list of unfinished tasks whose futures shutdown must drop.
    pub(crate) tasks: TaskList,
    /// Counts task allocations without keeping the queue's allocation alive.
    #[cfg(test)]
    pub(super) task_allocations: Arc<()>,
}

/// Aligns the next field to a cache line, 128 bytes covers the adjacent line
/// prefetcher on x86 and the line size on Apple cores.
#[derive(Debug)]
#[repr(align(128))]
struct CacheLine;

unsafe impl Send for RunnableQueue {}
unsafe impl Sync for RunnableQueue {}

impl RunnableQueue {
    /// Initializes an idle queue with this thread as its owner.
    pub(super) fn new(builder: &RuntimeBuilder, handle: Box<dyn Notify>) -> Self {
        let now = Instant::now();
        Self {
            handle,
            event_interval: builder.event_interval,
            id: current_thread_id(),
            runtime_id: wake::runtime_id(),
            idle: Cell::new(true),
            poll_start: Cell::new(now),
            self_wake_clock: Cell::new(None),
            notified: AtomicBool::new(false),
            admission: AtomicUsize::new(0),
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
            tasks: TaskList::new(),
            wake_queue: WakeQueue::new(),
            #[cfg(test)]
            task_allocations: Arc::new(()),
            _remote: [],
            _local: [],
        }
    }

    /// Spawns a task on this queue.
    ///
    /// # Safety
    ///
    /// The caller should ensure the captured lifetime is long enough.
    /// The caller must be on the runtime thread.
    pub(super) unsafe fn spawn<F: Future>(
        self: &Arc<Self>,
        future: F,
        location: SpawnLocation,
    ) -> JoinHandle<F::Output> {
        JoinHandle::new(unsafe { task::spawn(future, self, location) })
    }

    /// Spawns a task on this queue without a join handle.
    ///
    /// # Safety
    ///
    /// The caller should ensure the captured lifetime is long enough.
    /// If `LOCAL` is true, the caller must be on the runtime thread.
    /// Otherwise, the future must be `Send`.
    pub(super) unsafe fn spawn_detached<F: Future, const LOCAL: bool>(
        self: &Arc<Self>,
        future: F,
        location: SpawnLocation,
    ) {
        unsafe { task::spawn_detached::<_, LOCAL>(future, self, location) }
    }

    /// Enqueues a runnable on the owner thread and wakes an idle driver if needed.
    pub(crate) fn schedule_local(&self, runnable: Task) {
        self.local_queue.push(runnable);
        if self.idle.get() {
            // starts a busy period, `awake` does not reset it
            self.start_busy();
            // the poll running a task picks up tasks it schedules, a notify
            // could reenter the runtime and free the running task
            if self.current.load(Ordering::Relaxed).is_null() {
                self.notify();
            }
        }
    }

    /// Owner releases bypass the registry, including during shutdown cleanup.
    pub(crate) fn owner_entry(&self, entry: WakeEntry) {
        if self.admission.load(Ordering::Acquire) == CLOSED && self.wake_queue.is_empty() {
            drop(entry.into_runnable(self));
        } else {
            self.wake_queue.push(entry);
            if self.idle.get() && !self.is_closed() {
                self.start_busy();
                if self.current.load(Ordering::Relaxed).is_null() {
                    self.notify();
                }
            }
        }
    }

    /// Acquires remote wake publications without claiming another notification.
    pub(crate) fn acquire_wakes(&self) {
        // A later wake may have published writes behind an earlier FIFO entry.
        // Acquire the latest publication without resetting notification ownership.
        self.notified.fetch_or(false, Ordering::Acquire);
    }

    /// Notifies the driver, retrying interruptions and aborting on other failures.
    pub(super) fn notify(&self) {
        /// Aborts when a failed notification could strand queued work.
        #[cold]
        fn failed(err: &io::Error) -> ! {
            log::error!(target: LOG_TARGET, "Failed to notify runtime driver: {err}; aborting");
            std::process::abort();
        }

        // Aborts if the notifier unwinds, producers coalesced behind this
        // wakeup would never be woken.
        struct AbortOnUnwind;

        impl Drop for AbortOnUnwind {
            /// Prevents unwinding from abandoning a coalesced driver wakeup.
            fn drop(&mut self) {
                log::error!(target: LOG_TARGET, "Runtime notifier panicked; aborting");
                std::process::abort();
            }
        }

        let guard = AbortOnUnwind;
        // Keep responsibility for this wakeup: other producers may already
        // have coalesced behind it, even if no later submission arrives.
        while let Err(err) = self.handle.notify() {
            if err.kind() != io::ErrorKind::Interrupted {
                failed(&err);
            }
        }
        std::mem::forget(guard);
    }

    /// Reserves a submission or wake before publication or acquiring a task queue reference.
    /// `owned` keeps the queue alive if publication releases the caller's reference.
    pub(crate) fn admit(self: &Arc<Self>, owned: bool) -> Option<RemotePermit<'_>> {
        let mut state = self.admission.load(Ordering::Acquire);
        loop {
            if state & CLOSED != 0 {
                return None;
            }
            if state == CLOSED - 1 {
                std::process::abort();
            }
            match self.admission.compare_exchange_weak(
                state,
                state + 1,
                Ordering::Acquire,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(RemotePermit {
                        queue: Some(if owned {
                            Cow::Owned(self.clone())
                        } else {
                            Cow::Borrowed(self)
                        }),
                    });
                }
                Err(v) => state = v,
            }
        }
    }

    /// Runs bounded local, submission, and wake batches; returns whether work remains.
    pub(super) fn run(&self) -> bool {
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

        // Avoid the empty pop's fence. A racing publication is caught by the
        // final readiness check or its driver notification.
        if !self.wake_queue.is_empty() {
            for _ in 0..self.event_interval + REMOTE_BATCH {
                let Some(wake) = self.wake_queue.pop() else {
                    break;
                };
                if let Some(task) = wake.into_runnable(self) {
                    self.run_task(task);
                }
            }
        }

        // tasks from other threads may schedule local tasks
        let more_tasks = !self.local_queue.is_empty() || !self.wake_queue.is_empty();
        if !more_tasks {
            self.idle.set(true);
        }
        more_tasks
    }

    /// Runs a task while publishing its location and tracking self-wake streaks.
    fn run_task(&self, task: Task) {
        struct Reset<'a>(&'a AtomicPtr<Location<'static>>, *mut Location<'static>);

        impl Drop for Reset<'_> {
            /// Restores the enclosing task's location, including during unwinding.
            fn drop(&mut self) {
                // a task that polls the runtime runs tasks inside its own run
                self.0.store(self.1, Ordering::Relaxed);
            }
        }

        let stats = task.stats();
        // only the owner writes `current`; no callback can reenter between
        // saving it and publishing the new location
        let prev = self.current.load(Ordering::Relaxed);
        self.current.store(
            ptr::from_ref(stats.location()).cast_mut(),
            Ordering::Relaxed,
        );
        let wakes = if self.self_wake_limit == 0 {
            0
        } else {
            let wakes = stats.self_wakes();
            if wakes != 0 {
                stats.set_self_wakes(0);
            }
            wakes
        };
        let _reset = Reset(&self.current, prev);

        if task.run() {
            // woken while polled, the task is alive in `local_queue`
            self.self_wake(stats, wakes);
        }
    }

    /// Counts a self-wake and reports a streak once both configured limits are met.
    pub(super) fn self_wake(&self, task: impl TaskStats, wakes: u32) {
        let total = self.self_wakes.load(Ordering::Relaxed);
        self.self_wakes
            .store(total.wrapping_add(1), Ordering::Relaxed);

        if self.self_wake_limit == 0 {
            return;
        }

        let reported = wakes & REPORTED;
        let count = ((wakes & !REPORTED) + 1).min(MAX_SELF_WAKES);
        let now = self.self_wake_clock.get().unwrap_or_else(|| {
            // wraps after 49 days, longer streaks may be measured short
            #[allow(clippy::cast_possible_truncation)]
            let now = self
                .poll_start
                .get()
                .saturating_duration_since(self.epoch)
                .as_millis() as u32;
            self.self_wake_clock.set(Some(now));
            now
        });
        if count == 1 {
            task.set_streak_start(now);
        }

        let wakes = count | reported;
        if reported == 0 && count >= self.self_wake_limit.min(MAX_SELF_WAKES) {
            let elapsed = Duration::from_millis(u64::from(now.wrapping_sub(task.streak_start())));
            if elapsed >= self.self_wake_time {
                // Logging may reenter the runtime and free the queued task.
                task.set_self_wakes(wakes | REPORTED);
                let location = task.location();
                log::warn!(
                    target: LOG_TARGET,
                    "Task spawned at {location} woke itself {count} times in {elapsed:?} without waiting, possible busy loop"
                );
                return;
            }
        }
        task.set_self_wakes(wakes);
    }

    /// Starts a new turn timestamp and invalidates its cached streak-clock offset.
    pub(super) fn set_poll_start(&self, now: Instant) {
        self.poll_start.set(now);
        self.self_wake_clock.set(None);
    }

    /// Leaves the idle state and starts busy-time and current-turn accounting.
    pub(super) fn start_busy(&self) {
        let now = Instant::now();
        self.idle.set(false);
        self.set_poll_start(now);
        self.busy.start(now);
    }

    /// Counts a scheduler poll and, when `again` is true, its remaining backlog.
    pub(super) fn add_poll(&self, again: bool) {
        let polls = self.polls.load(Ordering::Relaxed);
        self.polls.store(polls.wrapping_add(1), Ordering::Relaxed);
        if again {
            let polls = self.polls_again.load(Ordering::Relaxed);
            self.polls_again
                .store(polls.wrapping_add(1), Ordering::Relaxed);
        }
    }

    /// Drains submissions, wake commands, and local runnables without polling futures.
    fn clear(&self) {
        while let Some(wake) = self.wake_queue.pop() {
            drop(wake.into_runnable(self));
        }
        // dropped tasks may schedule other tasks, drop each outside of the borrow
        while let Some(task) = self.pop_local() {
            drop(task);
        }
    }

    /// Drops the futures of all tasks, also of tasks waiting on a waker.
    pub(super) fn shutdown(&self) {
        /// Retries cleanup after its first panic and aborts on a second panic.
        fn cleanup(f: impl Fn(), first_panic: &mut Option<Box<dyn std::any::Any + Send>>) {
            while let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(&f)) {
                if first_panic.is_some() || std::thread::panicking() {
                    std::mem::forget(payload);
                    crate::abort_on_second_panic();
                }
                *first_panic = Some(payload);
            }
        }

        let mut first_panic = None;
        let previous = self.admission.fetch_or(CLOSED, Ordering::AcqRel);
        // Synchronize endpoint lookups before waiting for admitted publications.
        // A nested or repeated shutdown must not remove the outer cleanup access.
        let _shutdown = (previous & CLOSED == 0).then(|| wake::unregister(self.runtime_id));
        // A reserved submission may not have entered its queue yet.
        while self.admission.load(Ordering::Acquire) != CLOSED {
            yield_now();
        }
        // Each entry is removed or completed before its cleanup can unwind.
        // Retry the phase so remaining and callback-generated work is drained.
        cleanup(
            || loop {
                self.clear();
                if !self.tasks.close_first() {
                    break;
                }
            },
            &mut first_panic,
        );
        // All admitted publications are linked; rejected ones cannot enqueue.
        cleanup(|| self.clear(), &mut first_panic);
        if let Some(payload) = first_panic {
            std::panic::resume_unwind(payload);
        }
    }

    /// Returns whether shutdown has closed admission to new remote publications.
    pub(crate) fn is_closed(&self) -> bool {
        self.admission.load(Ordering::Acquire) & CLOSED != 0
    }

    /// Transfers the oldest local runnable's reference to the owner-thread caller.
    pub(super) fn pop_local(&self) -> Option<Task> {
        self.local_queue.pop()
    }

    #[cfg(test)]
    pub(super) fn task_count(&self) -> usize {
        Arc::strong_count(&self.task_allocations) - 1
    }
}

impl fmt::Debug for RunnableQueue {
    /// Formats immutable configuration and atomic state without reading owner-only cells.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunnableQueue")
            .field("id", &self.id)
            .field("epoch", &self.epoch)
            .field("self_wake_time", &self.self_wake_time)
            .field("event_interval", &self.event_interval)
            .field("self_wake_limit", &self.self_wake_limit)
            .field("notified", &self.notified.load(Ordering::Relaxed))
            .field("admission", &self.admission.load(Ordering::Relaxed))
            .field("polls", &self.polls.load(Ordering::Relaxed))
            .field("polls_again", &self.polls_again.load(Ordering::Relaxed))
            .field("self_wakes", &self.self_wakes.load(Ordering::Relaxed))
            .field("current", &self.current.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

pub(crate) struct RemotePermit<'a> {
    queue: Option<Cow<'a, Arc<RunnableQueue>>>,
}

impl RemotePermit<'_> {
    /// Publishes an admitted submission and notifies the driver when required.
    pub(crate) fn schedule(self, runnable: RemoteTask) {
        if let Some(queue) = self.publish(WakeEntry::spawn(runnable)) {
            queue.notify();
        }
    }

    /// Transfers a command before releasing admission; callbacks run after returning.
    pub(crate) fn publish(mut self, entry: WakeEntry) -> Option<Arc<RunnableQueue>> {
        self.queue.as_ref().unwrap().wake_queue.push(entry);
        self.published()
    }

    /// Releases admission and selects the producer responsible for notifying the driver.
    fn published(&mut self) -> Option<Arc<RunnableQueue>> {
        // A notifier may block, panic or reenter the runtime.
        let queue = self.queue.take().unwrap();
        queue.admission.fetch_sub(1, Ordering::Release);
        if queue.notified.swap(true, Ordering::Release) {
            None
        } else {
            Some(queue.into_owned())
        }
    }
}

impl Drop for RemotePermit<'_> {
    /// Releases a reserved publication if it was never submitted.
    fn drop(&mut self) {
        if let Some(queue) = &self.queue {
            queue.admission.fetch_sub(1, Ordering::Release);
        }
    }
}
