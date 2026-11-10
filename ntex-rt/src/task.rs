use std::{cell::Cell, future::Future, pin::Pin, task::Context, task::Poll};

thread_local!(
    // callbacks created by the current system's factory, set together with the current system
    static CALLBACKS: Cell<Option<TaskCallbacks>> = const { Cell::new(None) };
);

/// Callbacks for instrumenting spawned tasks.
///
/// Configure a factory returning them with
/// [`Builder::task_callbacks`](crate::Builder::task_callbacks) or the
/// `task_callbacks` argument of `#[ntex::main]` and `#[ntex::test]`. The factory
/// is called once on every thread of the system, when the thread joins it.
/// They apply when [`crate::spawn`], [`crate::spawn_detached`],
/// [`spawn_sendable`], [`crate::rt::spawn`], or [`crate::rt::spawn_detached`]
/// wraps a future on a thread of the configured system, including its
/// arbiters. They do not apply retroactively, to direct
/// [`crate::rt::Runtime`] spawning methods, or on threads without a system.
///
/// - `before` runs synchronously when the future is wrapped. Returning `None`
///   disables the remaining callbacks for that future.
/// - `enter` receives the task's pointer before each poll and returns a pointer
///   representing the previous context.
/// - `exit` receives that previous-context pointer after the poll, including
///   when polling unwinds.
/// - `after` receives the task's original pointer when its callback state is
///   dropped, including cancellation before the first poll.
///
/// Pointers are opaque to the runtime, which never dereferences them. Keep the
/// state returned by `before` valid until `after`, and the state returned by
/// `enter` valid until its matching `exit`. Callbacks of a thread run only on
/// that thread and may be called while other tasks are spawned.
#[derive(Copy, Clone, Debug)]
pub struct TaskCallbacks {
    /// Called when a future is wrapped, returns the task's pointer.
    pub before: fn() -> Option<*const ()>,
    /// Called before each poll, returns the previous context.
    pub enter: fn(*const ()) -> *const (),
    /// Called after each poll with the value returned by `enter`.
    pub exit: fn(*const ()),
    /// Called once with the task's pointer when its callback state is dropped.
    pub after: fn(*const ()),
}

/// Installs the current system's callbacks on this thread.
pub(crate) fn set_callbacks(cbs: Option<TaskCallbacks>) {
    CALLBACKS.set(cbs);
}

pub(crate) struct Data {
    cbs: TaskCallbacks,
    ptr: *const (),
}

impl Data {
    pub(crate) fn load() -> Option<Data> {
        let cbs = CALLBACKS.get()?;
        (cbs.before)().map(|ptr| Data { cbs, ptr })
    }

    pub(crate) fn run<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        struct Exit(fn(*const ()), *const ());

        impl Drop for Exit {
            fn drop(&mut self) {
                (self.0)(self.1);
            }
        }

        // `exit` also runs if `f` panics, so the context set by `enter` is restored
        let _exit = Exit(self.cbs.exit, (self.cbs.enter)(self.ptr));
        f()
    }
}

/// Wraps a future so that task callbacks, if registered, run around each poll.
///
/// Always returns the same future type, so spawning code is generated once
/// per future rather than once per callback mode.
pub(crate) fn wrap<F: Future>(fut: F) -> impl Future<Output = F::Output> {
    let mut data = Data::load();
    async move {
        let mut f = std::pin::pin!(fut);
        std::future::poll_fn(|cx| match data.as_mut() {
            Some(data) => data.run(|| f.as_mut().poll(cx)),
            None => f.as_mut().poll(cx),
        })
        .await
    }
}

impl Drop for Data {
    fn drop(&mut self) {
        (self.cbs.after)(self.ptr);
    }
}

/// Spawns a local future and returns a [`SendableJoinHandle`] for its result.
///
/// The task is spawned immediately on the current runtime, using
/// [`crate::spawn_detached`] and the current system's [`TaskCallbacks`]. The spawned
/// future need not be `Send`, but its output must be. Only the returned handle
/// may be moved to another thread; the task stays on its original runtime,
/// which must keep being driven.
///
/// Dropping the handle detaches the task unless
/// [`cancel_on_drop`](SendableJoinHandle::cancel_on_drop) is enabled.
/// Use [`cancel`](SendableJoinHandle::cancel) to request cancellation explicitly.
///
/// Awaiting the handle yields the output or [`crate::JoinError`] if
/// the task ends without one, for example on panic or runtime shutdown. Task
/// panics follow the runtime's usual behavior.
///
/// # Panics
///
/// Panics outside a runtime context that supports [`crate::spawn_detached`].
#[track_caller]
pub fn spawn_sendable<F>(fut: F) -> SendableJoinHandle<F::Output>
where
    F: Future + 'static,
    F::Output: Send,
{
    let (tx, rx) = oneshot::async_channel();
    let (cancel, canceled) = oneshot::async_channel::<()>();
    crate::spawn_detached(async move {
        let output = {
            let mut fut = std::pin::pin!(fut);
            let mut canceled = Some(canceled);
            std::future::poll_fn(|cx| {
                if let Some(rx) = &mut canceled {
                    match Pin::new(rx).poll(cx) {
                        Poll::Ready(Ok(())) => return Poll::Ready(None),
                        Poll::Ready(Err(_)) => canceled = None,
                        Poll::Pending => {}
                    }
                }
                fut.as_mut().poll(cx).map(Some)
            })
            .await
        };
        if let Some(output) = output {
            let _ = tx.send(output);
        }
    });
    SendableJoinHandle {
        rx,
        cancel: Some(cancel),
        cancel_on_drop: false,
    }
}

/// A sendable task handle returned by [`spawn_sendable()`].
///
/// The handle is `Send` when `T` is `Send`, but is not `Sync`. It may be moved
/// and polled on another thread while the task stays on its original runtime.
/// That runtime must keep being driven for the task to make progress.
///
/// Dropping the handle detaches the task unless
/// [`cancel_on_drop`](Self::cancel_on_drop) is enabled. Awaiting it yields the
/// output, or [`crate::JoinError`] if the task ends without one, including on
/// panic or runtime shutdown.
#[derive(Debug)]
pub struct SendableJoinHandle<T> {
    rx: oneshot::AsyncReceiver<T>,
    cancel: Option<oneshot::Sender<()>>,
    cancel_on_drop: bool,
}

impl<T> SendableJoinHandle<T> {
    /// Requests cancellation when this handle is dropped instead of detaching the task.
    ///
    /// Cancellation follows [`cancel`](Self::cancel) and does not wait for cleanup.
    /// A running poll is not interrupted.
    #[must_use]
    pub fn cancel_on_drop(mut self) -> Self {
        self.cancel_on_drop = true;
        self
    }

    /// Requests cancellation and discards the result without waiting for cleanup.
    ///
    /// The future is dropped on its runtime thread when the runtime next
    /// processes the task, or during shutdown. A running poll is not interrupted.
    /// This method may be called from another thread without a runtime context.
    pub fn cancel(mut self) {
        self.cancel_on_drop = true;
    }

    /// Returns `true` if the task completed or was closed without an output.
    pub fn is_finished(&self) -> bool {
        self.rx.has_message() || self.rx.is_closed()
    }
}

impl<T> Drop for SendableJoinHandle<T> {
    fn drop(&mut self) {
        if self.cancel_on_drop
            && let Some(cancel) = self.cancel.take()
        {
            let _ = cancel.send(());
        }
    }
}

impl<T> Future for SendableJoinHandle<T> {
    type Output = Result<T, crate::JoinError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.rx)
            .poll(cx)
            .map(|result| result.map_err(|_| crate::JoinError))
    }
}

#[cfg(test)]
#[path = "../tests/unit/task.rs"]
mod tests;
