use slab::Slab;
use std::{future::Future, pin::Pin, task::Context, task::Poll};

use super::cell::Cell;
use crate::{future::poll_fn, task::LocalWaker};

/// Condition allows to notify multiple waiters at the same time
#[derive(Clone, Debug)]
pub struct Condition(Cell<Inner>);

#[derive(Debug)]
struct Inner {
    data: Slab<Option<LocalWaker>>,
}

impl Default for Condition {
    fn default() -> Self {
        Self::new()
    }
}

impl Condition {
    /// Coonstruct new condition instance
    pub fn new() -> Condition {
        Condition(Cell::new(Inner { data: Slab::new() }))
    }

    /// Get condition waiter
    pub fn wait(&self) -> Waiter {
        let token = self.0.get_mut().data.insert(None);
        Waiter {
            token,
            inner: self.0.clone(),
        }
    }

    /// Notify all waiters
    pub fn notify(&self) {
        let inner = self.0.get_ref();
        for item in inner.data.iter() {
            if let Some(waker) = item.1 {
                waker.wake();
            }
        }
    }
}

impl Drop for Condition {
    fn drop(&mut self) {
        self.notify()
    }
}

#[derive(Debug)]
#[must_use = "Waiter do nothing unless polled"]
pub struct Waiter {
    token: usize,
    inner: Cell<Inner>,
}

impl Waiter {
    /// Returns readiness state of the condition.
    pub async fn ready(&self) {
        poll_fn(|cx| self.poll_ready(cx)).await
    }

    /// Returns readiness state of the condition.
    pub fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<()> {
        let inner = unsafe { self.inner.get_mut().data.get_unchecked_mut(self.token) };
        if inner.is_none() {
            let waker = LocalWaker::default();
            waker.register(cx.waker());
            *inner = Some(waker);
        } else if !inner.as_mut().unwrap().register(cx.waker()) {
            return Poll::Ready(());
        }
        Poll::Pending
    }
}

impl Clone for Waiter {
    fn clone(&self) -> Self {
        let token = self.inner.get_mut().data.insert(None);
        Waiter {
            token,
            inner: self.inner.clone(),
        }
    }
}

impl Future for Waiter {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.get_mut().poll_ready(cx)
    }
}

impl Drop for Waiter {
    fn drop(&mut self) {
        self.inner.get_mut().data.remove(self.token);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::future::lazy;

    #[ntex_macros::rt_test2]
    #[allow(clippy::unit_cmp)]
    async fn test_condition() {
        let cond = Condition::new();
        let mut waiter = cond.wait();
        assert_eq!(
            lazy(|cx| Pin::new(&mut waiter).poll(cx)).await,
            Poll::Pending
        );
        cond.notify();
        assert_eq!(waiter.await, ());

        let mut waiter = cond.wait();
        assert_eq!(
            lazy(|cx| Pin::new(&mut waiter).poll(cx)).await,
            Poll::Pending
        );
        let mut waiter2 = waiter.clone();
        assert_eq!(
            lazy(|cx| Pin::new(&mut waiter2).poll(cx)).await,
            Poll::Pending
        );

        drop(cond);
        assert_eq!(waiter.await, ());
        assert_eq!(waiter2.await, ());
    }

    #[ntex_macros::rt_test2]
    async fn test_condition_poll() {
        let cond = Condition::default().clone();
        let waiter = cond.wait();
        assert_eq!(lazy(|cx| waiter.poll_ready(cx)).await, Poll::Pending);
        cond.notify();
        waiter.ready().await;

        let waiter2 = waiter.clone();
        assert_eq!(lazy(|cx| waiter.poll_ready(cx)).await, Poll::Pending);
        assert_eq!(lazy(|cx| waiter2.poll_ready(cx)).await, Poll::Pending);

        drop(cond);
        assert_eq!(lazy(|cx| waiter.poll_ready(cx)).await, Poll::Ready(()));
        assert_eq!(lazy(|cx| waiter2.poll_ready(cx)).await, Poll::Ready(()));
    }
}
