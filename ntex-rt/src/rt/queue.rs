//! Run queues of a runtime, shared by the runtime and the tasks.
use std::borrow::Cow;
use std::cell::Cell;
use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use std::{fmt, future::Future, io, panic::Location, ptr, sync::Arc};

use crate::handle::{JoinHandle, SpawnLocation};
use crate::{busy::BusyClock, driver::Notify, metrics::AtomicHistogram};

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

use super::task::{
    self, LocalQueue, Remote as RemoteTask, RemoteQueue, Runnable as Task, TaskList, task_ref,
};
use super::{LOG_TARGET, REPORTED, RuntimeBuilder, TaskStats, ThreadId, current_thread_id};

const MAX_SELF_WAKES: u32 = task::MAX_SELF_WAKES;

/// Remote tasks run per turn, besides `event_interval` overflow tasks.
const REMOTE_BATCH: usize = 128;

/// The remaining bits count remote publications admitted before shutdown.
const CLOSED: usize = 1 << (usize::BITS - 1);

#[repr(C)]
pub(crate) struct RunnableQueue {
    // read-only, other threads read `id` on every waker operation
    pub(crate) id: ThreadId,
    pub(super) handle: Box<dyn Notify>,
    pub(super) epoch: Instant,
    self_wake_time: Duration,
    event_interval: usize,
    self_wake_limit: u32,
    // written by other threads
    _remote: [CacheLine; 0],
    // set by the first remote `schedule` after `run` cleared it
    notified: AtomicBool,
    // closed bit and in-flight remote publications
    admission: AtomicUsize,
    pub(super) remote_queue: RemoteQueue,
    // written by the runtime thread
    _local: [CacheLine; 0],
    pub(super) idle: Cell<bool>,
    // start of the current busy period, valid while not idle
    pub(super) poll_start: Cell<Instant>,
    // `poll_start` relative to the epoch, cached in milliseconds
    pub(super) self_wake_clock: Cell<Option<u32>>,
    pub(super) busy: BusyClock,
    // poll counters, written by the runtime thread only
    pub(super) polls: AtomicU64,
    pub(super) polls_again: AtomicU64,
    pub(super) poll_histogram: AtomicHistogram,
    // self-wake counter, written by the runtime thread only
    pub(super) self_wakes: AtomicU64,
    // spawn location of the running task, null between tasks
    pub(super) current: AtomicPtr<Location<'static>>,
    pub(super) local_queue: LocalQueue,
    // unfinished tasks, dropped with the runtime
    pub(crate) tasks: TaskList,
}

/// Aligns the next field to a cache line, 128 bytes covers the adjacent line
/// prefetcher on x86 and the line size on Apple cores.
#[derive(Debug)]
#[repr(align(128))]
pub(super) struct CacheLine;

unsafe impl Send for RunnableQueue {}
unsafe impl Sync for RunnableQueue {}

impl RunnableQueue {
    pub(super) fn new(builder: &RuntimeBuilder, handle: Box<dyn Notify>) -> Self {
        let now = Instant::now();
        Self {
            handle,
            event_interval: builder.event_interval,
            id: current_thread_id(),
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
    /// If `LOCAL` is true, the caller must be on the runtime thread.
    pub(super) unsafe fn spawn<F: Future, const LOCAL: bool>(
        self: &Arc<Self>,
        future: F,
        location: SpawnLocation,
    ) -> JoinHandle<F::Output> {
        JoinHandle::new(unsafe { task::spawn::<_, LOCAL>(future, self, location) })
    }

    /// Spawns a task on this queue without a join handle.
    ///
    /// # Safety
    ///
    /// The caller should ensure the captured lifetime is long enough.
    /// If `LOCAL` is true, the caller must be on the runtime thread.
    pub(super) unsafe fn spawn_detached<F: Future, const LOCAL: bool>(
        self: &Arc<Self>,
        future: F,
        location: SpawnLocation,
    ) {
        unsafe { task::spawn_detached::<_, LOCAL>(future, self, location) }
    }

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

    pub(super) fn notify(&self) {
        #[cold]
        fn failed(err: &io::Error) -> ! {
            log::error!(target: LOG_TARGET, "Failed to notify runtime driver: {err}; aborting");
            std::process::abort();
        }

        // Aborts if the notifier unwinds, producers coalesced behind this
        // wakeup would never be woken.
        struct AbortOnUnwind;

        impl Drop for AbortOnUnwind {
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

    /// Reserves a publication before a task acquires its remote queue reference.
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

    pub(super) fn set_poll_start(&self, now: Instant) {
        self.poll_start.set(now);
        self.self_wake_clock.set(None);
    }

    pub(super) fn start_busy(&self) {
        let now = Instant::now();
        self.idle.set(false);
        self.set_poll_start(now);
        self.busy.start(now);
    }

    pub(super) fn add_poll(&self, again: bool) {
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

    /// Drops the futures of all tasks, also of tasks waiting on a waker.
    pub(super) fn shutdown(&self) {
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
        self.admission.fetch_or(CLOSED, Ordering::AcqRel);
        // A reserved task may not have entered the MPSC queue yet.
        while self.admission.load(Ordering::Acquire) != CLOSED {
            yield_now();
        }
        // All admitted publications are linked; rejected ones cannot enqueue.
        cleanup(|| self.clear(), &mut first_panic);
        if let Some(payload) = first_panic {
            std::panic::resume_unwind(payload);
        }
    }

    /// The runtime is dropped, its tasks never run.
    pub(crate) fn is_closed(&self) -> bool {
        self.admission.load(Ordering::Acquire) & CLOSED != 0
    }

    pub(super) fn pop_local(&self) -> Option<Task> {
        self.local_queue.pop()
    }
}

impl fmt::Debug for RunnableQueue {
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
    pub(crate) fn schedule(mut self, runnable: RemoteTask) {
        self.queue.as_ref().unwrap().remote_queue.push(runnable);
        // A notifier may block, panic or reenter the runtime.
        let queue = self.queue.take().unwrap();
        queue.admission.fetch_sub(1, Ordering::Release);
        if !queue.notified.swap(true, Ordering::Release) {
            queue.notify();
        }
    }
}

impl Drop for RemotePermit<'_> {
    fn drop(&mut self) {
        if let Some(queue) = &self.queue {
            queue.admission.fetch_sub(1, Ordering::Release);
        }
    }
}

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
                Err(_) => yield_now(),
            }
        }
    }
}
