//! A thread pool for blocking operations.
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering, fence};
use std::task::{Context, Poll};
use std::{any::Any, fmt, future::Future, panic, pin::Pin, thread, time::Duration};

use crossbeam_channel::{Receiver, Select, Sender, TrySendError, bounded, unbounded};

/// Submits blocking work and returns a future for its result.
///
/// If a system is running, work is submitted to its blocking thread pool.
/// Otherwise, the closure runs immediately on the current thread.
///
/// Dropping the returned future prevents queued work from starting, but cannot
/// interrupt work that is already running. Call [`BlockingResult::detach`] to
/// let queued work continue even if its result is no longer needed.
pub fn spawn_blocking<F, R>(f: F) -> BlockingResult<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    if let Some(sys) = crate::System::try_current() {
        sys.spawn_blocking(f)
    } else {
        ThreadPool::execute_inplace(f)
    }
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
#[derive(Debug)]
pub struct BlockingResult<T> {
    rx: oneshot::AsyncReceiver<Result<T, Box<dyn Any + Send>>>,
}

impl<T: 'static> BlockingResult<T> {
    /// Detaches the task so it can continue without awaiting its result.
    pub fn detach(self) {
        crate::spawn(async move {
            let _ = self.await;
        })
        .detach();
    }
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

fn worker(
    receiver_high_prio: Receiver<BoxedDispatchable>,
    receiver_low_prio: Receiver<BoxedDispatchable>,
    guard: CounterGuard,
    thread_limit: usize,
    timeout: Duration,
) -> impl FnOnce() {
    move || {
        let mut guard = guard;
        let mut sel = Select::new_biased();
        sel.recv(&receiver_high_prio);
        sel.recv(&receiver_low_prio);
        loop {
            match sel.select_timeout(timeout) {
                Ok(op) if op.index() == 0 => {
                    if let Ok(f) = op.recv(&receiver_high_prio) {
                        f.run();
                    }
                }
                Ok(op) => {
                    if let Ok(f) = op.recv(&receiver_low_prio) {
                        f.run();
                    }
                }
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
                }
            }
        }
    }
}

/// A thread pool for executing blocking operations.
///
/// The pool can be configured as either bounded or unbounded, which
/// determines how tasks are handled when all worker threads are busy.
///
/// The number of worker threads scales dynamically with load, but will
/// never exceed the `thread_limit` parameter. When all worker threads are
/// busy, tasks are queued until a worker thread becomes available.
#[derive(Debug, Clone)]
pub struct ThreadPool {
    name: String,
    sender_low_prio: Sender<BoxedDispatchable>,
    receiver_low_prio: Receiver<BoxedDispatchable>,
    sender_high_prio: Sender<BoxedDispatchable>,
    receiver_high_prio: Receiver<BoxedDispatchable>,
    counter: Arc<AtomicUsize>,
    thread_limit: usize,
    recv_timeout: Duration,
}

impl ThreadPool {
    /// Creates a [`ThreadPool`] with a maximum number of worker threads
    /// and a timeout for receiving tasks from the task channel.
    ///
    /// A `thread_limit` of zero is treated as one.
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
            counter: Arc::new(AtomicUsize::new(0)),
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
        BlockingResult { rx }
    }

    #[allow(clippy::missing_panics_doc)]
    /// Submits a closure to the thread pool.
    ///
    /// The task will be executed by an available worker thread. If no threads
    /// are available and the pool has reached its maximum size, the work will
    /// be queued until a worker thread becomes available. This method never
    /// blocks.
    pub fn execute<F, R>(&self, f: F) -> BlockingResult<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        let (tx, rx) = oneshot::async_channel();
        let f = Box::new(move || {
            // do not execute operation if receiver is dropped
            if !tx.is_closed() {
                let result = panic::catch_unwind(panic::AssertUnwindSafe(f));
                let _ = tx.send(result);
            }
        });

        // hand over to an idle worker
        let f = match self.sender_low_prio.try_send(f) {
            Ok(()) => return BlockingResult { rx },
            Err(TrySendError::Full(f)) => f,
            Err(TrySendError::Disconnected(_)) => {
                unreachable!("receiver should not all disconnected")
            }
        };

        self.sender_high_prio
            .send(f)
            .expect("the channel should not be closed");
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
        }
        BlockingResult { rx }
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
mod tests {
    use std::time::Instant;

    use super::*;

    fn wait<R>(fut: BlockingResult<R>, timeout: Duration) -> Option<Result<R, BlockingError>> {
        let mut fut = std::pin::pin!(fut);
        let mut cx = Context::from_waker(std::task::Waker::noop());
        let start = Instant::now();
        loop {
            if let Poll::Ready(res) = fut.as_mut().poll(&mut cx) {
                return Some(res);
            }
            if start.elapsed() > timeout {
                return None;
            }
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn thread_limit_respected() {
        let pool = ThreadPool::new("test", 2, Duration::from_secs(1));
        let running = Arc::new(AtomicUsize::new(0));
        let max = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(std::sync::Barrier::new(8));

        // concurrent submitters
        let submitters: Vec<_> = (0..8)
            .map(|_| {
                let (pool, running, max, barrier) =
                    (pool.clone(), running.clone(), max.clone(), barrier.clone());
                thread::spawn(move || {
                    barrier.wait();
                    (0..8)
                        .map(|_| {
                            let (running, max) = (running.clone(), max.clone());
                            pool.execute(move || {
                                let cnt = running.fetch_add(1, Ordering::SeqCst) + 1;
                                max.fetch_max(cnt, Ordering::SeqCst);
                                thread::sleep(Duration::from_millis(5));
                                running.fetch_sub(1, Ordering::SeqCst);
                            })
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        for s in submitters {
            for res in s.join().unwrap() {
                assert_eq!(wait(res, Duration::from_secs(10)), Some(Ok(())));
            }
        }
        assert!(max.load(Ordering::SeqCst) <= 2, "{max:?} tasks ran at once");
    }

    #[test]
    fn idle_workers_do_not_strand_tasks() {
        let pool = ThreadPool::new("test", 1, Duration::from_millis(2));
        for i in 0..300u64 {
            // submit around the moment the idle worker times out
            thread::sleep(Duration::from_micros(1500 + (i % 10) * 100));
            let res = wait(pool.execute(move || i), Duration::from_secs(5));
            assert_eq!(res, Some(Ok(i)), "task {i} was not executed");
        }
    }

    #[test]
    fn zero_thread_limit() {
        let pool = ThreadPool::new("test", 0, Duration::from_secs(1));
        assert_eq!(
            wait(pool.execute(|| 1), Duration::from_secs(5)),
            Some(Ok(1))
        );
    }
}
