use std::sync::{Arc, atomic::AtomicU64, atomic::Ordering};
use std::task::{Context, Poll, ready};
use std::time::{Duration, Instant};
use std::{cell::Cell, cell::RefCell, fmt, future::Future, marker::PhantomData, pin::Pin, rc::Rc};

use crate::arbiter::Arbiter;
use crate::busy::BusyClock;
use crate::metrics::{AtomicHistogram, Histogram};
use crate::remote::{Job, RemoteJoin, Spawner};

thread_local! {
    static STOPPING: Cell<bool> = const { Cell::new(false) };
    // runtime counters of this thread
    static STATS: RefCell<Arc<Stats>> = RefCell::new(Arc::default());
    // start of the current runtime turn, `None` while the runtime waits for events
    static POLL_START: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// Returns the start of the current runtime turn.
///
/// Tasks polled in the same turn observe the same instant. Returns
/// [`Instant::now()`] outside of a runtime turn.
#[inline]
pub fn now() -> Instant {
    POLL_START.with(Cell::get).unwrap_or_else(Instant::now)
}

#[doc(hidden)]
/// Sets the start of the current runtime turn, see [`now()`].
///
/// Called by the compio driver.
pub fn set_poll_start(start: Option<Instant>) {
    POLL_START.with(|s| s.set(start));
}

// runtime counters, written by the runtime thread only
#[derive(Debug)]
struct Stats {
    busy: BusyClock,
    polls: AtomicU64,
    polls_again: AtomicU64,
    poll_histogram: AtomicHistogram,
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            busy: BusyClock::new(),
            polls: AtomicU64::new(0),
            polls_again: AtomicU64::new(0),
            poll_histogram: AtomicHistogram::default(),
        }
    }
}

fn incr(counter: &AtomicU64) {
    let v = counter.load(Ordering::Relaxed);
    counter.store(v.wrapping_add(1), Ordering::Relaxed);
}

#[doc(hidden)]
/// Resets the runtime counters of the current thread.
///
/// Called by the compio driver before it starts a new runtime.
pub fn reset_busy() {
    STATS.with(|s| *s.borrow_mut() = Arc::default());
}

#[doc(hidden)]
/// Starts a runtime turn on the current thread, it counts as busy time.
///
/// Called by the compio driver.
pub fn start_busy(start: Instant) {
    STATS.with(|s| s.borrow().busy.start(start));
}

#[doc(hidden)]
/// Ends the runtime turn on the current thread.
///
/// Called by the compio driver.
pub fn stop_busy(end: Instant) {
    STATS.with(|s| s.borrow().busy.stop(end));
}

#[doc(hidden)]
/// Counts a runtime turn of the current thread.
///
/// `busy` is the duration of the turn, `again` is set if scheduled tasks
/// remain after the turn.
pub fn add_poll(busy: Duration, again: bool) {
    STATS.with(|s| {
        let s = s.borrow();
        s.poll_histogram.record(busy);
        incr(&s.polls);
        if again {
            incr(&s.polls_again);
        }
    });
}

#[doc(hidden)]
/// Marks the compio runtime of the current thread as stopping.
///
/// While set, [`spawn()`] drops the future instead of spawning it. The runtime
/// drops its remaining tasks on shutdown, and a destructor that spawns a new
/// task would insert it into the task queue while the queue is being cleared.
pub fn set_stopping(stopping: bool) {
    STOPPING.with(|s| s.set(stopping));
}

/// Spawns a future on the current Compio runtime and returns its join handle.
///
/// The future and output need not be `Send`. Registered task callbacks are
/// applied. Dropping the handle detaches the task; the runtime must keep being
/// driven for it to make progress. An ntex system is not required.
///
/// If the runtime is shutting down, the future is dropped without running and
/// the returned handle resolves to [`JoinError`].
///
/// # Panics
///
/// Panics if no Compio runtime is active when a task must be spawned.
#[inline]
pub fn spawn<F>(f: F) -> JoinHandle<F::Output>
where
    F: Future + 'static,
{
    if STOPPING.with(Cell::get) {
        drop(f);
        return JoinHandle::new(None);
    }

    JoinHandle::new(Some(compio_runtime::spawn(crate::task::wrap(f))))
}

#[inline]
/// Spawns a future on the current thread without a join handle.
///
/// Like [`spawn()`], this applies registered task callbacks and supports
/// non-`Send` futures and outputs. The output is discarded on completion, and no
/// ntex join handle is returned. If the runtime is shutting down, the future
/// is dropped without running.
///
/// # Panics
///
/// Panics if no Compio runtime is active when a task must be spawned.
pub fn spawn_detached<F>(f: F)
where
    F: Future + 'static,
{
    if STOPPING.with(Cell::get) {
        drop(f);
        return;
    }
    compio_runtime::spawn(crate::task::wrap(f)).detach();
}

/// Error returned when a Compio-backed task ends without an output.
///
/// Cancellation, task panics, and rejected submissions during shutdown are
/// reported without preserving their cause.
#[derive(Debug, Copy, Clone)]
pub struct JoinError;

impl fmt::Display for JoinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "JoinError")
    }
}

impl std::error::Error for JoinError {}

/// A local task handle. It is neither `Send` nor `Sync`.
///
/// Returned by [`spawn()`]. Dropping the handle detaches the task.
/// Awaiting it returns the task output or [`JoinError`] if the task ends without one.
#[derive(Debug)]
pub struct JoinHandle<T> {
    task: Option<compio_runtime::JoinHandle<T>>,
    _local: PhantomData<Rc<()>>,
}

impl<T> JoinHandle<T> {
    fn new(task: Option<compio_runtime::JoinHandle<T>>) -> Self {
        Self {
            task,
            _local: PhantomData,
        }
    }

    /// Requests cancellation and discards the result without waiting for cleanup.
    ///
    /// A running poll is not interrupted; the runtime must make progress to
    /// finish cancellation of a pending task.
    pub fn cancel(mut self) {
        if let Some(t) = self.task.take() {
            drop(t.cancel());
        }
    }

    /// Detaches the task to let it keep running in the background.
    pub fn detach(self) {}

    /// Returns `true` if the task has finished or was rejected during shutdown.
    pub fn is_finished(&self) -> bool {
        match &self.task {
            Some(t) => t.is_finished(),
            None => true,
        }
    }
}

impl<T> Drop for JoinHandle<T> {
    fn drop(&mut self) {
        if let Some(t) = self.task.take() {
            t.detach();
        }
    }
}

impl<T> Future for JoinHandle<T> {
    type Output = Result<T, JoinError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Poll::Ready(match self.task.as_mut() {
            Some(t) => ready!(Pin::new(t).poll(cx)).map_err(|_| JoinError),
            None => Err(JoinError),
        })
    }
}

/// A task handle returned by [`Handle::spawn()`].
///
/// It is neither `Send` nor `Sync` and stays on the calling thread.
/// Submissions on the runtime thread retain the local spawn fast path.
/// Dropping the handle detaches the task.
/// Awaiting it returns the output or [`JoinError`] if the task ends without one,
/// including when submission was rejected during shutdown.
#[derive(Debug)]
pub struct RemoteJoinHandle<T> {
    inner: Inner<T>,
}

#[derive(Debug)]
enum Inner<T> {
    Local(JoinHandle<T>),
    Remote(RemoteJoin<T>),
}

impl<T> RemoteJoinHandle<T> {
    fn local(task: JoinHandle<T>) -> Self {
        Self {
            inner: Inner::Local(task),
        }
    }

    fn remote(join: RemoteJoin<T>) -> Self {
        Self {
            inner: Inner::Remote(join),
        }
    }

    /// Requests cancellation and discards the result without waiting for cleanup.
    ///
    /// Accepted futures are dropped on the runtime thread. A running poll is
    /// not interrupted, and the runtime must make progress to finish cancellation.
    pub fn cancel(self) {
        match self.inner {
            Inner::Local(t) => t.cancel(),
            Inner::Remote(t) => t.cancel(),
        }
    }

    /// Detaches the task to let it keep running in the background.
    pub fn detach(self) {}

    /// Returns `true` if the task completed or ended without an output.
    pub fn is_finished(&self) -> bool {
        match &self.inner {
            Inner::Local(t) => t.is_finished(),
            Inner::Remote(t) => t.is_finished(),
        }
    }
}

impl<T> Future for RemoteJoinHandle<T> {
    type Output = Result<T, JoinError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Poll::Ready(match &mut self.inner {
            Inner::Local(t) => ready!(Pin::new(t).poll(cx)),
            Inner::Remote(t) => ready!(t.poll_join(cx)).ok_or(JoinError),
        })
    }
}

/// A thread-safe handle for submitting work to a Compio-backed arbiter.
///
/// Clones share the same target arbiter and its metrics.
#[derive(Clone, Debug)]
pub struct Handle {
    spawner: Arc<Spawner>,
    stats: Arc<Stats>,
}

impl Handle {
    /// Must be called on the arbiter thread, inside its runtime.
    pub(crate) fn new() -> Self {
        let stats = STATS.with(|s| s.borrow().clone());
        Self {
            spawner: Spawner::start(),
            stats,
        }
    }

    /// Returns a handle to the current arbiter's runtime.
    ///
    /// # Panics
    ///
    /// Panics if no arbiter is installed on the current thread. A Compio runtime
    /// context alone is not sufficient.
    pub fn current() -> Self {
        Arbiter::current().handle().clone()
    }

    /// Compatibility hook that does nothing on the Compio backend.
    ///
    /// Task submission and task wakers notify Compio automatically.
    pub fn notify(&self) {}

    /// Returns the total time the runtime has been busy.
    ///
    /// The runtime is busy while it polls tasks, time spent waiting for and
    /// handling driver events is not counted. Includes the current turn, so
    /// it can be read from other threads while a task runs. Always returns
    /// `Some`, the return type matches the other runtime backends.
    pub fn busy_duration(&self) -> Option<Duration> {
        Some(self.stats.busy.busy_duration())
    }

    /// Returns how long the current runtime turn has been running, `None`
    /// while the runtime is idle or handles driver events.
    pub fn current_turn(&self) -> Option<Duration> {
        self.stats.busy.current_turn()
    }

    /// Returns the number of runtime turns.
    ///
    /// Always returns `Some`, the return type matches the other runtime
    /// backends.
    pub fn poll_count(&self) -> Option<u64> {
        Some(self.stats.polls.load(Ordering::Relaxed))
    }

    /// Returns the number of runtime turns that left scheduled tasks.
    ///
    /// The `poll_again_count` delta divided by the
    /// [`poll_count`](Self::poll_count) delta over an interval is the share of
    /// turns that left a backlog. Always returns `Some`.
    pub fn poll_again_count(&self) -> Option<u64> {
        Some(self.stats.polls_again.load(Ordering::Relaxed))
    }

    /// Returns the number of times a task woke itself while it was polled, not
    /// available on Compio.
    ///
    /// Always returns `None`, the method exists for compatibility with the
    /// native runtime.
    pub fn self_wake_count(&self) -> Option<u64> {
        None
    }

    /// Returns the spawn location of the task the runtime is polling, not
    /// available on Compio.
    ///
    /// Always returns `None`, the method exists for compatibility with the
    /// native runtime.
    pub fn current_task(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    /// Returns the histogram of runtime turn durations.
    ///
    /// Always returns `Some`.
    pub fn poll_histogram(&self) -> Option<Histogram> {
        Some(self.stats.poll_histogram.snapshot())
    }

    /// Spawns a task on the handle's runtime and returns a [`RemoteJoinHandle`].
    ///
    /// There is no guarantee that a spawned task will execute to completion.
    /// Both the future and output must be `Send`, even when submitted on the
    /// runtime thread, which uses the local spawn fast path. Accepted futures
    /// are polled and dropped on the runtime thread, with the output transferred
    /// through the join handle. Rejected submissions are never polled and their
    /// futures may be dropped on the submitting thread.
    ///
    /// The returned join handle is neither `Send` nor `Sync`; dropping it
    /// detaches the task.
    ///
    /// # Panics
    ///
    /// Panics on the owner thread if local spawning needs a runtime and no
    /// Compio runtime is active. Other threads do not require a runtime.
    pub fn spawn<F>(&self, future: F) -> RemoteJoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        if self.spawner.is_owner() {
            RemoteJoinHandle::local(spawn(future))
        } else {
            let job = Job::new(future, true);
            let join = RemoteJoin::new(job.clone());
            self.spawner.submit(job);
            RemoteJoinHandle::remote(join)
        }
    }

    /// Spawns a new asynchronous task without a join handle.
    ///
    /// The output is dropped on the runtime thread when the task completes,
    /// so it does not have to be `Send`.
    /// The future must be `Send`, even on the owner thread. Rejected submissions
    /// are never polled and their futures may be dropped on the submitting thread.
    ///
    /// # Panics
    ///
    /// Panics on the owner thread if local spawning needs a runtime and no
    /// Compio runtime is active. Other threads do not require a runtime.
    pub fn spawn_detached<F>(&self, future: F)
    where
        F: Future + Send + 'static,
    {
        if self.spawner.is_owner() {
            spawn_detached(future);
        } else {
            self.spawner.submit(Job::new(
                async move {
                    future.await;
                },
                false,
            ));
        }
    }
}
