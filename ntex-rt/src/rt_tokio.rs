use std::{future::Future, future::poll_fn, pin::Pin, task::Context, task::Poll};
pub use tok_io::task::JoinError;

#[inline]
/// Spawn a future on the current thread.
///
/// This does not create a new Arbiter or Arbiter address,
/// it is simply a helper for spawning futures on the current thread.
///
/// # Panics
///
/// This function panics if ntex system is not running.
pub fn spawn<F>(f: F) -> JoinHandle<F::Output>
where
    F: Future + 'static,
{
    let task = if let Some(mut data) = crate::task::Data::load() {
        tok_io::task::spawn_local(async move {
            tok_io::pin!(f);
            poll_fn(|cx| data.run(|| f.as_mut().poll(cx))).await
        })
    } else {
        tok_io::task::spawn_local(f)
    };

    JoinHandle { task }
}

#[derive(Debug)]
/// A spawned task.
pub struct JoinHandle<T> {
    task: tok_io::task::JoinHandle<T>,
}

impl<T> JoinHandle<T> {
    /// Cancels the task and waits for it to stop running
    pub fn cancel(self) {
        self.task.abort();
    }

    /// Detaches the task to let it keep running in the background
    pub fn detach(self) {}

    /// Returns true if the current task is finished
    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }
}

impl<T> Future for JoinHandle<T> {
    type Output = Result<T, JoinError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.task).poll(cx)
    }
}

#[derive(Clone, Debug)]
/// Handle to the runtime.
pub struct Handle(tok_io::runtime::Handle);

impl Handle {
    #[inline]
    pub fn current() -> Self {
        Self(tok_io::runtime::Handle::current())
    }

    #[inline]
    /// Wake up runtime.
    pub fn notify(&self) {}

    #[inline]
    /// Spawns a new asynchronous task, returning a [`Task`] for it.
    ///
    /// Spawning a task enables the task to execute concurrently to other tasks.
    /// There is no guarantee that a spawned task will execute to completion.
    pub fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        JoinHandle {
            task: self.0.spawn(future),
        }
    }
}
