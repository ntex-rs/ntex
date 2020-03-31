use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use slab::Slab;

use super::cell::Cell;
use crate::task::LocalWaker;

/// Condition allows to notify multiple receivers at the same time
pub struct Condition(Cell<Inner>);

struct Inner {
    data: Slab<Option<LocalWaker>>,
}

impl Default for Condition {
    fn default() -> Self {
        Self::new()
    }
}

impl Condition {
    pub fn new() -> Condition {
        Condition(Cell::new(Inner { data: Slab::new() }))
    }

    /// Get condition waiter
    pub fn wait(&mut self) -> Waiter {
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

#[must_use = "Waiter do nothing unless polled"]
pub struct Waiter {
    token: usize,
    inner: Cell<Inner>,
}

impl Waiter {
    pub fn poll_waiter(&self, cx: &mut Context<'_>) -> Poll<()> {
        let inner = unsafe {
            self.inner
                .get_mut_unchecked()
                .data
                .get_unchecked_mut(self.token)
        };
        if inner.is_none() {
            let waker = LocalWaker::default();
            waker.register(cx.waker());
            *inner = Some(waker);
            Poll::Pending
        } else if inner.as_mut().unwrap().register(cx.waker()) {
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    }
}

impl Clone for Waiter {
    fn clone(&self) -> Self {
        let token = unsafe { self.inner.get_mut_unchecked() }.data.insert(None);
        Waiter {
            token,
            inner: self.inner.clone(),
        }
    }
}

impl Future for Waiter {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        let inner = unsafe { this.inner.get_mut().data.get_unchecked_mut(this.token) };
        if inner.is_none() {
            let waker = LocalWaker::default();
            waker.register(cx.waker());
            *inner = Some(waker);
            Poll::Pending
        } else if inner.as_mut().unwrap().register(cx.waker()) {
            Poll::Pending
        } else {
            Poll::Ready(())
        }
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
    use futures::future::lazy;

    #[ntex_rt::test]
    async fn test_condition() {
        let mut cond = Condition::new();
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

    #[ntex_rt::test]
    async fn test_condition_poll() {
        let mut cond = Condition::new();
        let waiter = cond.wait();
        assert_eq!(lazy(|cx| waiter.poll_waiter(cx)).await, Poll::Pending);
        cond.notify();
        assert_eq!(lazy(|cx| waiter.poll_waiter(cx)).await, Poll::Ready(()));

        let waiter = cond.wait();
        assert_eq!(lazy(|cx| waiter.poll_waiter(cx)).await, Poll::Pending);
        let waiter2 = waiter.clone();
        assert_eq!(lazy(|cx| waiter2.poll_waiter(cx)).await, Poll::Pending);

        drop(cond);
        assert_eq!(lazy(|cx| waiter.poll_waiter(cx)).await, Poll::Ready(()));
        assert_eq!(lazy(|cx| waiter2.poll_waiter(cx)).await, Poll::Ready(()));
    }
}
