use std::sync::{Arc, atomic::AtomicU64, atomic::Ordering};
use std::task::{Context, Poll, ready};
use std::time::{Duration, Instant};
use std::{cell::Cell, cell::RefCell, fmt, future::Future, pin::Pin};

use async_channel::Sender;

use crate::arbiter::{Arbiter, ArbiterCommand};
use crate::busy::BusyClock;
use crate::metrics::{AtomicHistogram, Histogram};

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

/// Spawn a future on the current thread.
///
/// This does not create a new Arbiter
/// or Arbiter address, it is simply a helper for spawning futures on the current
/// thread.
///
/// # Panics
///
/// This function panics if ntex system is not running.
///
/// If the runtime is shutting down, the future is dropped without running and
/// the returned handle resolves to [`JoinError`].
#[inline]
pub fn spawn<F>(f: F) -> JoinHandle<F::Output>
where
    F: Future + 'static,
{
    if STOPPING.with(Cell::get) {
        drop(f);
        return JoinHandle { task: None };
    }

    let task = compio_runtime::spawn(crate::task::wrap(f));

    JoinHandle {
        task: Some(Either::Compio(task)),
    }
}

#[inline]
/// Spawns a future on the current thread without a join handle.
///
/// The output is dropped when the task completes. Unlike dropping the
/// [`JoinHandle`] returned by [`spawn()`], this does not allocate a handle.
///
/// # Panics
///
/// This function panics if ntex system is not running.
///
/// If the runtime is shutting down, the future is dropped without running.
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

#[derive(Debug, Copy, Clone)]
pub struct JoinError;

impl fmt::Display for JoinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "JoinError")
    }
}

impl std::error::Error for JoinError {}

#[derive(Debug)]
enum Either<T> {
    Compio(compio_runtime::JoinHandle<T>),
    Spawn(crate::task::RemoteJoin<T>),
}

#[derive(Debug)]
pub struct JoinHandle<T> {
    task: Option<Either<T>>,
}

impl<T> JoinHandle<T> {
    /// Cancels the task.
    pub fn cancel(mut self) {
        match self.task.take() {
            Some(Either::Compio(fut)) => drop(fut.cancel()),
            Some(Either::Spawn(fut)) => fut.cancel(),
            None => (),
        }
    }

    /// Detaches the task to let it keep running in the background.
    pub fn detach(mut self) {
        if let Some(Either::Compio(fut)) = self.task.take() {
            fut.detach();
        }
    }

    /// Returns true if the current task is finished.
    pub fn is_finished(&self) -> bool {
        match &self.task {
            Some(Either::Compio(fut)) => fut.is_finished(),
            Some(Either::Spawn(fut)) => fut.is_finished(),
            None => true,
        }
    }
}

impl<T> Drop for JoinHandle<T> {
    fn drop(&mut self) {
        if let Some(Either::Compio(fut)) = self.task.take() {
            fut.detach();
        }
    }
}

impl<T> Future for JoinHandle<T> {
    type Output = Result<T, JoinError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Poll::Ready(match self.task.as_mut() {
            Some(Either::Compio(fut)) => {
                let mut res = Pin::new(&mut *fut).poll(cx);
                // compio-executor loses the wakeup if the task completes on
                // its thread while the handle is polled from another thread
                // and sets its waker, the completion is visible afterwards
                if res.is_pending() && fut.is_finished() {
                    res = Pin::new(fut).poll(cx);
                }
                ready!(res).map_err(|_| JoinError)
            }
            Some(Either::Spawn(fut)) => ready!(Pin::new(fut).poll(cx)).map_err(|_| JoinError),
            None => Err(JoinError),
        })
    }
}

#[derive(Clone, Debug)]
pub struct Handle {
    sender: Sender<ArbiterCommand>,
    stats: Arc<Stats>,
}

impl Handle {
    /// Must be called on the arbiter thread.
    pub(crate) fn new(sender: Sender<ArbiterCommand>) -> Self {
        let stats = STATS.with(|s| s.borrow().clone());
        Self { sender, stats }
    }

    pub fn current() -> Self {
        Arbiter::current().handle().clone()
    }

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
    /// available on compio.
    ///
    /// Always returns `None`, the method exists for compatibility with the
    /// neon runtime.
    pub fn self_wake_count(&self) -> Option<u64> {
        None
    }

    /// Returns the spawn location of the task the runtime is polling, not
    /// available on compio.
    ///
    /// Always returns `None`, the method exists for compatibility with the
    /// neon runtime.
    pub fn current_task(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    /// Returns the histogram of runtime turn durations.
    ///
    /// Always returns `Some`.
    pub fn poll_histogram(&self) -> Option<Histogram> {
        Some(self.stats.poll_histogram.snapshot())
    }

    pub fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        JoinHandle {
            task: Some(Either::Spawn(crate::task::RemoteJoin::spawn(
                &self.sender,
                future,
            ))),
        }
    }

    /// Spawns a new asynchronous task without a [`JoinHandle`].
    ///
    /// The output is dropped on the runtime thread when the task completes,
    /// so it does not have to be `Send`.
    pub fn spawn_detached<F>(&self, future: F)
    where
        F: Future + Send + 'static,
    {
        let _ = self
            .sender
            .try_send(ArbiterCommand::Execute(Box::pin(async move {
                future.await;
            })));
    }
}
