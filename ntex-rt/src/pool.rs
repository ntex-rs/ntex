//! A thread pool for blocking operations.
use std::sync::{Arc, atomic::AtomicUsize, atomic::Ordering};
use std::task::{Context, Poll};
use std::{any::Any, fmt, future::Future, panic, pin::Pin, thread, time::Duration};

use crossbeam_channel::{Receiver, Select, Sender, TrySendError, bounded, unbounded};

/// An error that may be emitted when all worker threads are busy.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct BlockingError;

impl std::error::Error for BlockingError {}

impl fmt::Display for BlockingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        "All threads are busy".fmt(f)
    }
}

#[derive(Debug)]
pub struct BlockingResult<T> {
    rx: oneshot::AsyncReceiver<Result<T, Box<dyn Any + Send>>>,
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

struct CounterGuard(Arc<AtomicUsize>);

impl Drop for CounterGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn worker(
    receiver_high_prio: Receiver<BoxedDispatchable>,
    receiver_low_prio: Receiver<BoxedDispatchable>,
    counter: Arc<AtomicUsize>,
    timeout: Duration,
) -> impl FnOnce() {
    move || {
        counter.fetch_add(1, Ordering::AcqRel);
        let _guard = CounterGuard(counter);
        let mut sel = Select::new_biased();
        sel.recv(&receiver_high_prio);
        sel.recv(&receiver_low_prio);
        while let Ok(op) = sel.select_timeout(timeout) {
            match op {
                op if op.index() == 0 => {
                    if let Ok(f) = op.recv(&receiver_high_prio) {
                        f.run();
                    }
                }
                op if op.index() == 1 => {
                    if let Ok(f) = op.recv(&receiver_low_prio) {
                        f.run();
                    }
                }
                _ => unreachable!(),
            };
        }
    }
}

/// A thread pool for executing blocking operations.
///
/// The pool can be configured as either bounded or unbounded, which
/// determines how tasks are handled when all worker threads are busy.
///
/// - In a **bounded** pool, submitting a task will fail if the number of
///   concurrent operations has reached the thread limit.
/// - In an **unbounded** pool, tasks are queued and will wait until a
///   worker thread becomes available.
///
/// The number of worker threads scales dynamically with load, but will
/// never exceed the `thread_limit` parameter.
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
    pub fn new(name: &str, thread_limit: usize, recv_timeout: Duration) -> Self {
        let (sender_low_prio, receiver_low_prio) = bounded(0);
        let (sender_high_prio, receiver_high_prio) = unbounded();
        Self {
            sender_low_prio,
            receiver_low_prio,
            sender_high_prio,
            receiver_high_prio,
            thread_limit,
            recv_timeout,
            name: format!("{name}:pool-wrk"),
            counter: Arc::new(AtomicUsize::new(0)),
        }
    }

    #[allow(clippy::missing_panics_doc)]
    /// Submits a task (closure) to the thread pool.
    ///
    /// The task will be executed by an available worker thread.
    /// If no threads are available and the pool has reached its maximum size,
    /// the work will be queued until a worker thread becomes available.
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

        match self.sender_low_prio.try_send(f) {
            Ok(()) => BlockingResult { rx },
            Err(e) => match e {
                TrySendError::Full(f) => {
                    let cnt = self.counter.load(Ordering::Acquire);
                    if cnt >= self.thread_limit {
                        self.sender_high_prio
                            .send(f)
                            .expect("the channel should not be full");
                        BlockingResult { rx }
                    } else {
                        thread::Builder::new()
                            .name(format!("{}:{}", self.name, cnt))
                            .spawn(worker(
                                self.receiver_high_prio.clone(),
                                self.receiver_low_prio.clone(),
                                self.counter.clone(),
                                self.recv_timeout,
                            ))
                            .expect("Cannot construct new thread");
                        self.sender_low_prio
                            .send(f)
                            .expect("the channel should not be full");
                        BlockingResult { rx }
                    }
                }
                TrySendError::Disconnected(_) => {
                    unreachable!("receiver should not all disconnected")
                }
            },
        }
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
