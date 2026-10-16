use std::sync::{Arc, atomic::AtomicU64, atomic::Ordering};
use std::task::{Context, Poll, ready};
use std::time::{Duration, Instant};
use std::{cell::Cell, cell::RefCell, fmt, future::Future, pin::Pin};

use async_channel::Sender;

use crate::arbiter::{Arbiter, ArbiterCommand};

thread_local! {
    static STOPPING: Cell<bool> = const { Cell::new(false) };
    // total busy time in nanoseconds, written by the runtime thread only
    static BUSY: RefCell<Arc<AtomicU64>> = RefCell::new(Arc::default());
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

#[doc(hidden)]
/// Resets the busy time counter of the current thread.
///
/// Called by the compio driver before it starts a new runtime.
pub fn reset_busy() {
    BUSY.with(|b| *b.borrow_mut() = Arc::default());
}

#[doc(hidden)]
/// Adds the duration of a runtime turn to the busy time of the current thread.
pub fn add_busy(d: Duration) {
    let nanos = u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
    BUSY.with(|b| {
        let b = b.borrow();
        let total = b.load(Ordering::Relaxed).saturating_add(nanos);
        b.store(total, Ordering::Relaxed);
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
    Spawn(oneshot::AsyncReceiver<T>),
}

#[derive(Debug)]
pub struct JoinHandle<T> {
    task: Option<Either<T>>,
}

impl<T> JoinHandle<T> {
    /// Cancels the task.
    pub fn cancel(mut self) {
        if let Some(Either::Compio(fut)) = self.task.take() {
            drop(fut.cancel());
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
            Some(Either::Spawn(fut)) => fut.is_closed(),
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
            Some(Either::Compio(fut)) => ready!(Pin::new(fut).poll(cx)).map_err(|_| JoinError),
            Some(Either::Spawn(fut)) => ready!(Pin::new(fut).poll(cx)).map_err(|_| JoinError),
            None => Err(JoinError),
        })
    }
}

#[derive(Clone, Debug)]
pub struct Handle {
    sender: Sender<ArbiterCommand>,
    busy: Arc<AtomicU64>,
}

impl Handle {
    /// Must be called on the arbiter thread.
    pub(crate) fn new(sender: Sender<ArbiterCommand>) -> Self {
        let busy = BUSY.with(|b| b.borrow().clone());
        Self { sender, busy }
    }

    pub fn current() -> Self {
        Arbiter::current().handle().clone()
    }

    pub fn notify(&self) {}

    /// Returns the total time the runtime has been busy.
    ///
    /// The runtime is busy while it polls tasks, time spent waiting for and
    /// handling driver events is not counted. Always returns `Some`, the
    /// return type matches the other runtime backends.
    pub fn busy_duration(&self) -> Option<Duration> {
        Some(Duration::from_nanos(self.busy.load(Ordering::Relaxed)))
    }

    pub fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let (tx, rx) = oneshot::async_channel();

        let _ = self
            .sender
            .try_send(ArbiterCommand::Execute(Box::pin(async move {
                let result = future.await;
                let _ = tx.send(result);
            })));
        JoinHandle {
            task: Some(Either::Spawn(rx)),
        }
    }
}
