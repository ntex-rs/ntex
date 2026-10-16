use std::sync::Arc;
use std::task::{Context, Poll, ready};
use std::time::{Duration, Instant};
use std::{cell::Cell, cell::RefCell, fmt, future::Future, pin::Pin};

use async_channel::Sender;

use crate::arbiter::{Arbiter, ArbiterCommand};
use crate::busy::BusyClock;

thread_local! {
    static STOPPING: Cell<bool> = const { Cell::new(false) };
    // busy time of the runtime on this thread
    static BUSY: RefCell<Arc<BusyClock>> = RefCell::new(Arc::new(BusyClock::new()));
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
    BUSY.with(|b| *b.borrow_mut() = Arc::new(BusyClock::new()));
}

#[doc(hidden)]
/// Starts a runtime turn on the current thread, it counts as busy time.
///
/// Called by the compio driver.
pub fn start_busy(start: Instant) {
    BUSY.with(|b| b.borrow().start(start));
}

#[doc(hidden)]
/// Ends the runtime turn on the current thread.
///
/// Called by the compio driver.
pub fn stop_busy(end: Instant) {
    BUSY.with(|b| b.borrow().stop(end));
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
    busy: Arc<BusyClock>,
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
    /// handling driver events is not counted. Includes the current turn, so
    /// it can be read from other threads while a task runs. Always returns
    /// `Some`, the return type matches the other runtime backends.
    pub fn busy_duration(&self) -> Option<Duration> {
        Some(self.busy.busy_duration())
    }

    /// Returns how long the current runtime turn has been running, `None`
    /// while the runtime is idle or handles driver events.
    pub fn current_turn(&self) -> Option<Duration> {
        self.busy.current_turn()
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
