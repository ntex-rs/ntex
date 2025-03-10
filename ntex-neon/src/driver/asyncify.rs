use std::sync::{atomic::AtomicUsize, atomic::Ordering, Arc};
use std::{fmt, time::Duration};

use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};

/// An error that may be emitted when all worker threads are busy. It simply
/// returns the dispatchable value with a convenient [`fmt::Debug`] and
/// [`fmt::Display`] implementation.
#[derive(Copy, Clone, PartialEq, Eq)]
pub struct DispatchError<T>(pub T);

impl<T> DispatchError<T> {
    /// Consume the error, yielding the dispatchable that failed to be sent.
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> fmt::Debug for DispatchError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        "DispatchError(..)".fmt(f)
    }
}

impl<T> fmt::Display for DispatchError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        "all threads are busy".fmt(f)
    }
}

impl<T> std::error::Error for DispatchError<T> {}

type BoxedDispatchable = Box<dyn Dispatchable + Send>;

/// A trait for dispatching a closure. It's implemented for all `FnOnce() + Send
/// + 'static` but may also be implemented for any other types that are `Send`
///   and `'static`.
pub trait Dispatchable: Send + 'static {
    /// Run the dispatchable
    fn run(self: Box<Self>);
}

impl<F> Dispatchable for F
where
    F: FnOnce() + Send + 'static,
{
    fn run(self: Box<Self>) {
        (*self)()
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

/// A thread pool to perform blocking operations in other threads.
#[derive(Debug, Clone)]
pub struct AsyncifyPool {
    sender: Sender<BoxedDispatchable>,
    receiver: Receiver<BoxedDispatchable>,
    counter: Arc<AtomicUsize>,
    thread_limit: usize,
    recv_timeout: Duration,
}

impl AsyncifyPool {
    /// Create [`AsyncifyPool`] with thread number limit and channel receive
    /// timeout.
    pub fn new(thread_limit: usize, recv_timeout: Duration) -> Self {
        let (sender, receiver) = bounded(0);
        Self {
            sender,
            receiver,
            counter: Arc::new(AtomicUsize::new(0)),
            thread_limit,
            recv_timeout,
        }
    }

    /// Send a dispatchable, usually a closure, to another thread. Usually the
    /// user should not use it. When all threads are busy and thread number
    /// limit has been reached, it will return an error with the original
    /// dispatchable.
    pub fn dispatch<D: Dispatchable>(&self, f: D) -> Result<(), DispatchError<D>> {
        match self.sender.try_send(Box::new(f) as BoxedDispatchable) {
            Ok(_) => Ok(()),
            Err(e) => match e {
                TrySendError::Full(f) => {
                    if self.counter.load(Ordering::Acquire) >= self.thread_limit {
                        // Safety: we can ensure the type
                        Err(DispatchError(*unsafe {
                            Box::from_raw(Box::into_raw(f).cast())
                        }))
                    } else {
                        std::thread::spawn(worker(
                            self.receiver.clone(),
                            self.counter.clone(),
                            self.recv_timeout,
                        ));
                        self.sender.send(f).expect("the channel should not be full");
                        Ok(())
                    }
                }
                TrySendError::Disconnected(_) => {
                    unreachable!("receiver should not all disconnected")
                }
            },
        }
    }
}
