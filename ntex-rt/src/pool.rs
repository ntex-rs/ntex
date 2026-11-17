//! A thread pool for blocking operations.
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering, fence};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use std::{any::Any, fmt, future::Future, panic, pin::Pin, thread};

use crossbeam_channel::{Receiver, Select, Sender, TrySendError, bounded, unbounded};

/// Submits blocking work and returns a future for its result.
///
/// If a system is running, work is submitted to the blocking thread pool of
/// the current arbiter, see [`System::spawn_blocking`](crate::System::spawn_blocking).
/// Otherwise, the closure runs immediately on the current thread.
/// Panics from the closure are captured and reported as [`BlockingError`]
/// through the returned future in either case.
///
/// Dropping the returned future prevents queued work from starting, but cannot
/// interrupt work that is already running. Call [`BlockingResult::detach`] to
/// let queued work continue even if its result is no longer needed.
pub fn spawn_blocking<F, R>(f: F) -> BlockingResult<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    crate::System::with_current(|sys| match sys {
        Some(sys) => sys.spawn_blocking(f),
        None => ThreadPool::execute_inplace(f),
    })
}

/// Error returned when blocking work cannot produce a result.
///
/// This can occur if the task is canceled, panics, or no worker thread
/// can be started.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct BlockingError;

impl std::error::Error for BlockingError {}

impl fmt::Display for BlockingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        "Blocking task failed or was canceled".fmt(f)
    }
}

/// Future resolving to the result of blocking work.
///
/// Resolves to `Ok(output)` or [`BlockingError`] if the work cannot produce a
/// result, including a closure panic. Dropping this value cancels work that has
/// not been picked up by a worker; it does not interrupt running work. Use
/// [`detach`](Self::detach) to discard the result without canceling queued work.
#[derive(Debug)]
pub struct BlockingResult<T> {
    rx: oneshot::AsyncReceiver<Result<T, Box<dyn Any + Send>>>,
    detached: Option<Arc<AtomicBool>>,
}

impl<T> BlockingResult<T> {
    /// Detaches the task so it can continue without awaiting its result.
    ///
    /// Does not require a running runtime.
    pub fn detach(self) {
        if let Some(detached) = &self.detached {
            // published by dropping the receiver
            detached.store(true, Ordering::Relaxed);
        }
    }
}

/// Snapshot of [`ThreadPool`] metrics.
///
/// Durations and task counts are totals since the pool was created. Rates
/// and averages over an interval are computed from the deltas of two
/// snapshots, e.g. the average queue wait is the `wait_duration` delta
/// divided by the `tasks` delta.
///
/// [`System::pool_metrics`](crate::System::pool_metrics) combines the pools
/// of all arbiters: gauges, limits, and totals are summed and
/// `max_queue_depth` is the highest of any single pool.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct PoolMetrics {
    /// Maximum number of worker threads.
    pub thread_limit: usize,
    /// Number of worker threads, busy or idle.
    pub threads: usize,
    /// Number of tasks being executed.
    pub active_tasks: usize,
    /// Number of tasks waiting for a worker thread.
    pub queue_depth: usize,
    /// Highest number of tasks waiting for a worker thread at once.
    pub max_queue_depth: usize,
    /// Number of tasks queued while the pool was at its thread limit.
    ///
    /// A growing value means the pool has too few worker threads for its load.
    pub saturated_tasks: u64,
    /// Number of tasks started.
    pub tasks: u64,
    /// Total time started tasks waited for a worker thread.
    pub wait_duration: Duration,
    /// Total time worker threads spent executing tasks.
    pub busy_duration: Duration,
}

impl PoolMetrics {
    /// Adds the totals of another pool, keeping the highest queue depth.
    pub(crate) fn add_totals(&mut self, other: &PoolMetrics) {
        self.max_queue_depth = self.max_queue_depth.max(other.max_queue_depth);
        self.saturated_tasks = self.saturated_tasks.saturating_add(other.saturated_tasks);
        self.tasks = self.tasks.saturating_add(other.tasks);
        self.wait_duration = self.wait_duration.saturating_add(other.wait_duration);
        self.busy_duration = self.busy_duration.saturating_add(other.busy_duration);
    }

    /// Adds the limits, gauges, and totals of another pool.
    pub(crate) fn add(&mut self, other: &PoolMetrics) {
        self.thread_limit = self.thread_limit.saturating_add(other.thread_limit);
        self.threads += other.threads;
        self.active_tasks += other.active_tasks;
        self.queue_depth += other.queue_depth;
        self.add_totals(other);
    }
}

#[derive(Debug, Default)]
struct PoolStats {
    active: AtomicUsize,
    max_queue: AtomicUsize,
    saturated: AtomicU64,
    tasks: AtomicU64,
    // nanoseconds
    wait: AtomicU64,
    busy: AtomicU64,
}

fn as_nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

type BoxedDispatchable = Box<dyn Dispatchable + Send>;

pub(crate) trait Dispatchable: Send + 'static {
    fn run(self: Box<Self>);
}

impl<F> Dispatchable for F
where
    F: FnOnce() + Send + 'static,
{
    fn run(self: Box<Self>) {
        (*self)();
    }
}

/// Reserved worker slot, released on drop.
struct CounterGuard(Arc<AtomicUsize>);

impl CounterGuard {
    fn reserve(counter: &Arc<AtomicUsize>, limit: usize) -> Option<(Self, usize)> {
        counter
            .try_update(Ordering::AcqRel, Ordering::Acquire, |cnt| {
                (cnt < limit).then_some(cnt + 1)
            })
            .ok()
            .map(|cnt| (CounterGuard(counter.clone()), cnt))
    }
}

impl Drop for CounterGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Runs a task, keeping the worker alive if result delivery or disposal panics.
fn run_task(f: BoxedDispatchable) {
    if let Err(payload) = panic::catch_unwind(panic::AssertUnwindSafe(|| f.run())) {
        log::error!("Blocking pool task panicked during completion");
        // the payload destructor may panic as well
        std::mem::forget(payload);
    }
}

/// CPU affinity of the thread that created a pool.
///
/// A new thread inherits the affinity of the thread that starts it, usually a
/// pinned arbiter submitting work. Workers restore the creator's affinity.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[derive(Debug, Clone)]
struct CpuMask(Option<Arc<[libc::c_ulong]>>);

#[cfg(any(target_os = "linux", target_os = "android"))]
impl CpuMask {
    /// Words of a `cpu_set_t`.
    const SET_WORDS: usize = size_of::<libc::cpu_set_t>() / size_of::<libc::c_ulong>();

    /// Returns the CPU mask of the current thread.
    fn current() -> Self {
        let mut words = Self::SET_WORDS;
        loop {
            let mut mask = vec![0; words];
            // SAFETY: the kernel writes at most `size_of_val(mask)` bytes,
            // the mask has the layout of a larger `cpu_set_t`
            let res = unsafe {
                libc::sched_getaffinity(0, size_of_val(&mask[..]), mask.as_mut_ptr().cast())
            };
            if res == 0 {
                return Self(Some(mask.into()));
            }
            // the kernel mask is larger than `cpu_set_t`
            let einval = std::io::Error::last_os_error().raw_os_error() == Some(libc::EINVAL);
            if !einval || words >= Self::SET_WORDS << 10 {
                return Self(None);
            }
            words *= 2;
        }
    }

    /// Sets the CPU mask of the current thread.
    fn apply(&self) {
        if let Some(ref mask) = self.0 {
            // on failure, for example after a cpuset change, the thread keeps
            // the inherited mask
            // SAFETY: the kernel reads `size_of_val(mask)` bytes, the mask has
            // the layout of a larger `cpu_set_t`
            let _ =
                unsafe { libc::sched_setaffinity(0, size_of_val(&mask[..]), mask.as_ptr().cast()) };
        }
    }
}

/// Other platforms do not restore the creator's affinity.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
#[derive(Debug, Clone)]
struct CpuMask;

#[cfg(not(any(target_os = "linux", target_os = "android")))]
impl CpuMask {
    fn current() -> Self {
        CpuMask
    }

    #[allow(clippy::unused_self)]
    fn apply(&self) {}
}

fn worker(
    receiver_high_prio: Receiver<BoxedDispatchable>,
    receiver_low_prio: Receiver<BoxedDispatchable>,
    guard: CounterGuard,
    thread_limit: usize,
    timeout: Duration,
    affinity: CpuMask,
) -> impl FnOnce() {
    move || {
        affinity.apply();
        let mut guard = guard;
        let mut sel = Select::new_biased();
        sel.recv(&receiver_high_prio);
        sel.recv(&receiver_low_prio);
        loop {
            let task = match sel.select_timeout(timeout) {
                Ok(op) if op.index() == 0 => op.recv(&receiver_high_prio),
                Ok(op) => op.recv(&receiver_low_prio),
                Err(_) => {
                    // release the slot, then pick up work queued meanwhile,
                    // pairs with the fence in `ThreadPool::execute`
                    let counter = guard.0.clone();
                    drop(guard);
                    fence(Ordering::SeqCst);
                    if receiver_high_prio.is_empty() {
                        return;
                    }
                    match CounterGuard::reserve(&counter, thread_limit) {
                        Some((g, _)) => guard = g,
                        None => return,
                    }
                    continue;
                }
            };
            if let Ok(f) = task {
                run_task(f);
            } else {
                // the pool is dropped and the channels stay selectable,
                // run the queued work and exit
                while let Ok(f) = receiver_high_prio.try_recv() {
                    run_task(f);
                }
                return;
            }
        }
    }
}

/// A thread pool for executing blocking operations.
///
/// The number of worker threads scales dynamically with load, but will
/// never exceed the `thread_limit` parameter. When all worker threads are
/// busy, tasks enter an unbounded queue until a worker becomes available.
/// Clones share the workers, queue, and metrics.
///
/// On Linux and Android, workers run with the CPU affinity of the thread that
/// created the pool, not of the thread whose submission started them.
#[derive(Debug, Clone)]
pub struct ThreadPool {
    name: String,
    affinity: CpuMask,
    sender_low_prio: Sender<BoxedDispatchable>,
    receiver_low_prio: Receiver<BoxedDispatchable>,
    sender_high_prio: Sender<BoxedDispatchable>,
    receiver_high_prio: Receiver<BoxedDispatchable>,
    counter: Arc<AtomicUsize>,
    stats: Arc<PoolStats>,
    thread_limit: usize,
    recv_timeout: Duration,
}

impl ThreadPool {
    /// Creates a pool with a worker limit and idle-worker timeout.
    ///
    /// Workers are started on demand and named using `name`. An idle worker
    /// exits after `recv_timeout` without work. A `thread_limit` of zero is
    /// treated as one.
    pub fn new(name: &str, thread_limit: usize, recv_timeout: Duration) -> Self {
        let (sender_low_prio, receiver_low_prio) = bounded(0);
        let (sender_high_prio, receiver_high_prio) = unbounded();
        Self {
            sender_low_prio,
            receiver_low_prio,
            sender_high_prio,
            receiver_high_prio,
            thread_limit: thread_limit.max(1),
            recv_timeout,
            name: format!("{name}:pool-wrk"),
            affinity: CpuMask::current(),
            counter: Arc::new(AtomicUsize::new(0)),
            stats: Arc::default(),
        }
    }

    /// Returns a snapshot of the pool metrics.
    ///
    /// Gauges and totals are read independently and may be slightly
    /// inconsistent with each other.
    pub fn metrics(&self) -> PoolMetrics {
        PoolMetrics {
            thread_limit: self.thread_limit,
            threads: self.counter.load(Ordering::Relaxed),
            active_tasks: self.stats.active.load(Ordering::Relaxed),
            queue_depth: self.receiver_high_prio.len(),
            max_queue_depth: self.stats.max_queue.load(Ordering::Relaxed),
            saturated_tasks: self.stats.saturated.load(Ordering::Relaxed),
            tasks: self.stats.tasks.load(Ordering::Relaxed),
            wait_duration: Duration::from_nanos(self.stats.wait.load(Ordering::Relaxed)),
            busy_duration: Duration::from_nanos(self.stats.busy.load(Ordering::Relaxed)),
        }
    }

    pub(crate) fn execute_inplace<F, R>(f: F) -> BlockingResult<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        let (tx, rx) = oneshot::async_channel();
        let result = panic::catch_unwind(panic::AssertUnwindSafe(f));
        let _ = tx.send(result);
        BlockingResult { rx, detached: None }
    }

    #[allow(clippy::missing_panics_doc)]
    /// Submits a closure to the thread pool.
    ///
    /// The task will be executed by an available worker thread. If no threads
    /// are available and the pool has reached its maximum size, the work will
    /// be queued until a worker thread becomes available. Submission does not
    /// wait for the closure to run or for a worker slot, though it may start an
    /// OS thread.
    ///
    /// Dropping the result cancels work not yet picked up by a worker; use
    /// [`BlockingResult::detach`] to keep that work queued. Closure panics and
    /// failures to start a worker when none is available yield [`BlockingError`].
    pub fn execute<F, R>(&self, f: F) -> BlockingResult<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        let (tx, rx) = oneshot::async_channel();
        let detached = Arc::new(AtomicBool::new(false));
        let res = BlockingResult {
            rx,
            detached: Some(detached.clone()),
        };
        let stats = self.stats.clone();
        let queued = Instant::now();
        let f = Box::new(move || {
            // do not execute operation if receiver is dropped, unless detached,
            // the fence pairs with the receiver drop in `detach`
            if !tx.is_closed() || {
                fence(Ordering::Acquire);
                detached.load(Ordering::Relaxed)
            } {
                let start = Instant::now();
                stats.tasks.fetch_add(1, Ordering::Relaxed);
                stats
                    .wait
                    .fetch_add(as_nanos(start - queued), Ordering::Relaxed);
                stats.active.fetch_add(1, Ordering::Relaxed);

                let result = panic::catch_unwind(panic::AssertUnwindSafe(f));

                stats.active.fetch_sub(1, Ordering::Relaxed);
                stats
                    .busy
                    .fetch_add(as_nanos(start.elapsed()), Ordering::Relaxed);
                let _ = tx.send(result);
            }
        });

        // hand over to an idle worker
        let f = match self.sender_low_prio.try_send(f) {
            Ok(()) => return res,
            Err(TrySendError::Full(f)) => f,
            Err(TrySendError::Disconnected(_)) => {
                unreachable!("receiver should not all disconnected")
            }
        };

        self.sender_high_prio
            .send(f)
            .expect("the channel should not be closed");
        self.stats
            .max_queue
            .fetch_max(self.receiver_high_prio.len(), Ordering::Relaxed);
        // pairs with the fence in `worker`, either an exiting worker sees
        // the queued task or a free slot is visible here
        fence(Ordering::SeqCst);

        if let Some((guard, idx)) = CounterGuard::reserve(&self.counter, self.thread_limit) {
            let result = thread::Builder::new()
                .name(format!("{}:{}", self.name, idx))
                .spawn(worker(
                    self.receiver_high_prio.clone(),
                    self.receiver_low_prio.clone(),
                    guard,
                    self.thread_limit,
                    self.recv_timeout,
                    self.affinity.clone(),
                ));
            if let Err(e) = result {
                log::error!("Cannot start blocking pool thread: {e}");
                // no worker can run queued tasks, drop them so they
                // resolve with `BlockingError`
                while self.counter.load(Ordering::Acquire) == 0 {
                    if self.receiver_high_prio.try_recv().is_err() {
                        break;
                    }
                }
            }
        } else {
            self.stats.saturated.fetch_add(1, Ordering::Relaxed);
        }
        res
    }
}

impl<R> Future for BlockingResult<R> {
    type Output = Result<R, BlockingError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        match Pin::new(&mut this.rx).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => Poll::Ready(
                result
                    .map_err(|_| BlockingError)
                    .and_then(|res| res.map_err(|_| BlockingError)),
            ),
        }
    }
}

#[cfg(test)]
#[path = "../tests/unit/pool.rs"]
mod tests;
