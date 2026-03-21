//! A thread pool for blocking operations.
use std::sync::{Arc, atomic::AtomicUsize, atomic::Ordering};
use std::task::{Context, Poll};
use std::{any::Any, fmt, future::Future, panic, pin::Pin, thread, time::Duration};

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded, unbounded};

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
    rx: Option<oneshot::AsyncReceiver<Result<T, Box<dyn Any + Send>>>>,
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
    receiver: Receiver<BoxedDispatchable>,
    counter: Arc<AtomicUsize>,
    timeout: Duration,
) -> impl FnOnce() {
    move || {
        counter.fetch_add(1, Ordering::AcqRel);
        let _guard = CounterGuard(counter);
        while let Ok(f) = receiver.recv_timeout(timeout) {
            f.run();
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
    sender: Sender<BoxedDispatchable>,
    receiver: Receiver<BoxedDispatchable>,
    counter: Arc<AtomicUsize>,
    thread_limit: usize,
    recv_timeout: Duration,
}

impl ThreadPool {
    /// Creates a [`ThreadPool`] with a maximum number of worker threads
    /// and a timeout for receiving tasks from the task channel.
    pub fn new(
        name: &str,
        thread_limit: usize,
        recv_timeout: Duration,
        bound: bool,
    ) -> Self {
        let (sender, receiver) = if bound { bounded(0) } else { unbounded() };
        Self {
            sender,
            receiver,
            thread_limit,
            recv_timeout,
            name: format!("{name}:pool-wrk"),
            counter: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Submits a task (closure) to the thread pool.
    ///
    /// The task will be executed by an available worker thread.
    /// If no threads are available and the pool has reached its maximum size,
    /// the behavior depends on the `boundedness` configuration:
    ///
    /// - For a bounded pool, the function returns an error.
    /// - For an unbounded pool, the task is queued and executed when a worker
    ///   becomes available.
    pub fn execute<F, R>(&self, f: F) -> BlockingResult<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        let (tx, rx) = oneshot::async_channel();
        let f = Box::new(move || {
            // do not execute operation if recevier is dropped
            if !tx.is_closed() {
                let result = panic::catch_unwind(panic::AssertUnwindSafe(f));
                let _ = tx.send(result);
            }
        });

        match self.sender.try_send(f) {
            Ok(()) => BlockingResult { rx: Some(rx) },
            Err(e) => match e {
                TrySendError::Full(f) => {
                    let cnt = self.counter.load(Ordering::Acquire);
                    if cnt >= self.thread_limit {
                        BlockingResult { rx: None }
                    } else {
                        thread::Builder::new()
                            .name(format!("{}:{}", self.name, cnt))
                            .spawn(worker(
                                self.receiver.clone(),
                                self.counter.clone(),
                                self.recv_timeout,
                            ))
                            .expect("Cannot construct new thread");
                        self.sender.send(f).expect("the channel should not be full");
                        BlockingResult { rx: Some(rx) }
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

        if this.rx.is_none() {
            return Poll::Ready(Err(BlockingError));
        }

        if let Some(mut rx) = this.rx.take() {
            match Pin::new(&mut rx).poll(cx) {
                Poll::Pending => {
                    this.rx = Some(rx);
                    Poll::Pending
                }
                Poll::Ready(result) => Poll::Ready(
                    result
                        .map_err(|_| BlockingError)
                        .and_then(|res| res.map_err(|_| BlockingError)),
                ),
            }
        } else {
            unreachable!()
        }
    }
}
