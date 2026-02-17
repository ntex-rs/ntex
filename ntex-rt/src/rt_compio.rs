use std::task::{Context, Poll, ready};
use std::{fmt, future::Future, future::poll_fn, pin::Pin};

use async_channel::Sender;

use crate::arbiter::{Arbiter, ArbiterCommand};

/// Spawn a future on the current thread.
///
/// This does not create a new Arbiter
/// or Arbiter address, it is simply a helper for spawning futures on the current
/// thread.
///
/// # Panics
///
/// This function panics if ntex system is not running.
#[inline]
pub fn spawn<F>(f: F) -> JoinHandle<F::Output>
where
    F: Future + 'static,
{
    let task = if let Some(mut data) = crate::task::Data::load() {
        compio_runtime::spawn(async move {
            let mut f = std::pin::pin!(f);
            poll_fn(|cx| data.run(|| f.as_mut().poll(cx))).await
        })
    } else {
        compio_runtime::spawn(f)
    };

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
    Spawn(oneshot::Receiver<T>),
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
            Some(Either::Compio(fut)) => {
                ready!(Pin::new(fut).poll(cx)).map_err(|_| JoinError)
            }
            Some(Either::Spawn(fut)) => {
                ready!(Pin::new(fut).poll(cx)).map_err(|_| JoinError)
            }
            None => Err(JoinError),
        })
    }
}

#[derive(Clone, Debug)]
pub struct Handle(Sender<ArbiterCommand>);

impl Handle {
    pub(crate) fn new(sender: Sender<ArbiterCommand>) -> Self {
        Self(sender)
    }

    pub fn current() -> Self {
        Self(Arbiter::current().sender.clone())
    }

    pub fn notify(&self) {}

    pub fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let (tx, rx) = oneshot::channel();

        let _ = self
            .0
            .try_send(ArbiterCommand::Execute(Box::pin(async move {
                let result = future.await;
                let _ = tx.send(result);
            })));
        JoinHandle {
            task: Some(Either::Spawn(rx)),
        }
    }
}
