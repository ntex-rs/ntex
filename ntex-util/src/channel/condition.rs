use std::{
    cell, fmt, future::poll_fn, future::Future, pin::Pin, task::Context, task::Poll,
};

use slab::Slab;

use super::cell::Cell;
use crate::task::LocalWaker;

/// Condition allows to notify multiple waiters at the same time
pub struct Condition<T = ()>
where
    T: Default,
{
    inner: Cell<Inner<T>>,
}

struct Inner<T> {
    data: Slab<Option<Item<T>>>,
    ready: bool,
    count: usize,
}

struct Item<T> {
    val: cell::Cell<T>,
    waker: LocalWaker,
}

impl<T: Default> Default for Condition<T> {
    fn default() -> Self {
        Condition {
            inner: Cell::new(Inner {
                data: Slab::new(),
                ready: false,
                count: 1,
            }),
        }
    }
}

impl<T: Default> Clone for Condition<T> {
    fn clone(&self) -> Self {
        let inner = self.inner.clone();
        inner.get_mut().count += 1;
        Self { inner }
    }
}

impl<T: Default> fmt::Debug for Condition<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Condition")
            .field("ready", &self.inner.get_ref().ready)
            .finish()
    }
}

impl Condition<()> {
    /// Coonstruct new condition instance
    pub fn new() -> Condition<()> {
        Condition {
            inner: Cell::new(Inner {
                data: Slab::new(),
                ready: false,
                count: 1,
            }),
        }
    }
}

impl<T: Default> Condition<T> {
    /// Get condition waiter
    pub fn wait(&self) -> Waiter<T> {
        let token = self.inner.get_mut().data.insert(None);
        Waiter {
            token,
            inner: self.inner.clone(),
        }
    }

    /// Notify all waiters
    pub fn notify(&self) {
        let inner = self.inner.get_ref();
        for (_, item) in inner.data.iter() {
            if let Some(item) = item {
                item.waker.wake();
            }
        }
    }

    #[doc(hidden)]
    /// Notify all waiters.
    ///
    /// All subsequent waiter readiness checks always returns `ready`
    pub fn notify_and_lock_readiness(&self) {
        self.inner.get_mut().ready = true;
        self.notify();
    }
}

impl<T: Clone + Default> Condition<T> {
    /// Notify all waiters
    pub fn notify_with(&self, val: T) {
        let inner = self.inner.get_ref();
        for (_, item) in inner.data.iter() {
            if let Some(item) = item {
                if item.waker.wake_checked() {
                    item.val.set(val.clone());
                }
            }
        }
    }

    #[doc(hidden)]
    /// Notify all waiters.
    ///
    /// All subsequent waiter readiness checks always returns `ready`
    pub fn notify_with_and_lock_readiness(&self, val: T) {
        self.inner.get_mut().ready = true;
        self.notify_with(val);
    }
}

impl<T: Default> Drop for Condition<T> {
    fn drop(&mut self) {
        let inner = self.inner.get_mut();
        inner.count -= 1;
        if inner.count == 0 {
            self.notify_and_lock_readiness()
        }
    }
}

/// Waits for result from condition
pub struct Waiter<T = ()> {
    token: usize,
    inner: Cell<Inner<T>>,
}

impl<T: Default> Waiter<T> {
    /// Returns readiness state of the condition.
    pub async fn ready(&self) -> T {
        poll_fn(|cx| self.poll_ready(cx)).await
    }

    /// Returns readiness state of the condition.
    pub fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<T> {
        let parent = self.inner.get_mut();

        let inner = unsafe { parent.data.get_unchecked_mut(self.token) };
        if inner.is_none() {
            let waker = LocalWaker::default();
            waker.register(cx.waker());
            *inner = Some(Item {
                waker,
                val: cell::Cell::new(Default::default()),
            });
        } else {
            let item = inner.as_mut().unwrap();
            if !item.waker.register(cx.waker()) {
                return Poll::Ready(item.val.replace(Default::default()));
            }
        }
        if parent.ready {
            Poll::Ready(Default::default())
        } else {
            Poll::Pending
        }
    }
}

impl<T> Clone for Waiter<T> {
    fn clone(&self) -> Self {
        let token = self.inner.get_mut().data.insert(None);
        Waiter {
            token,
            inner: self.inner.clone(),
        }
    }
}

impl<T: Default> Future for Waiter<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.get_mut().poll_ready(cx)
    }
}

impl<T: Default> fmt::Debug for Waiter<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Waiter").finish()
    }
}

impl<T> Drop for Waiter<T> {
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
        assert!(format!("{cond:?}").contains("Condition"));
        assert!(format!("{waiter:?}").contains("Waiter"));
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
        assert_eq!(lazy(|cx| waiter.poll_ready(cx)).await, Poll::Pending);
        assert_eq!(lazy(|cx| waiter2.poll_ready(cx)).await, Poll::Pending);
        assert_eq!(lazy(|cx| waiter2.poll_ready(cx)).await, Poll::Pending);

        drop(cond);
        assert_eq!(lazy(|cx| waiter.poll_ready(cx)).await, Poll::Ready(()));
        assert_eq!(lazy(|cx| waiter.poll_ready(cx)).await, Poll::Ready(()));
        assert_eq!(lazy(|cx| waiter2.poll_ready(cx)).await, Poll::Ready(()));
        assert_eq!(lazy(|cx| waiter2.poll_ready(cx)).await, Poll::Ready(()));
    }

    #[ntex_macros::rt_test2]
    async fn test_condition_with() {
        let cond = Condition::<String>::default();
        let waiter = cond.wait();
        assert_eq!(lazy(|cx| waiter.poll_ready(cx)).await, Poll::Pending);
        cond.notify_with("TEST".into());
        assert_eq!(waiter.ready().await, "TEST".to_string());

        let waiter2 = waiter.clone();
        assert_eq!(lazy(|cx| waiter.poll_ready(cx)).await, Poll::Pending);
        assert_eq!(lazy(|cx| waiter.poll_ready(cx)).await, Poll::Pending);
        assert_eq!(lazy(|cx| waiter2.poll_ready(cx)).await, Poll::Pending);
        assert_eq!(lazy(|cx| waiter2.poll_ready(cx)).await, Poll::Pending);

        drop(cond);
        assert_eq!(
            lazy(|cx| waiter.poll_ready(cx)).await,
            Poll::Ready("".into())
        );
        assert_eq!(
            lazy(|cx| waiter.poll_ready(cx)).await,
            Poll::Ready("".into())
        );
        assert_eq!(
            lazy(|cx| waiter2.poll_ready(cx)).await,
            Poll::Ready("".into())
        );
        assert_eq!(
            lazy(|cx| waiter2.poll_ready(cx)).await,
            Poll::Ready("".into())
        );
    }

    #[ntex_macros::rt_test2]
    async fn notify_ready() {
        let cond = Condition::default().clone();
        let waiter = cond.wait();
        assert_eq!(lazy(|cx| waiter.poll_ready(cx)).await, Poll::Pending);

        cond.notify_and_lock_readiness();
        assert_eq!(lazy(|cx| waiter.poll_ready(cx)).await, Poll::Ready(()));
        assert_eq!(lazy(|cx| waiter.poll_ready(cx)).await, Poll::Ready(()));
        assert_eq!(lazy(|cx| waiter.poll_ready(cx)).await, Poll::Ready(()));

        let waiter2 = cond.wait();
        assert_eq!(lazy(|cx| waiter2.poll_ready(cx)).await, Poll::Ready(()));
    }

    #[ntex_macros::rt_test2]
    async fn notify_with_and_lock_ready() {
        // with
        let cond = Condition::<String>::default();
        let waiter = cond.wait();
        let waiter2 = cond.wait();
        assert_eq!(lazy(|cx| waiter.poll_ready(cx)).await, Poll::Pending);
        assert_eq!(lazy(|cx| waiter2.poll_ready(cx)).await, Poll::Pending);

        cond.notify_with_and_lock_readiness("TEST".into());
        assert_eq!(
            lazy(|cx| waiter.poll_ready(cx)).await,
            Poll::Ready("TEST".into())
        );
        assert_eq!(
            lazy(|cx| waiter.poll_ready(cx)).await,
            Poll::Ready("".into())
        );
        assert_eq!(
            lazy(|cx| waiter.poll_ready(cx)).await,
            Poll::Ready("".into())
        );
        assert_eq!(
            lazy(|cx| waiter2.poll_ready(cx)).await,
            Poll::Ready("TEST".into())
        );
        assert_eq!(
            lazy(|cx| waiter2.poll_ready(cx)).await,
            Poll::Ready("".into())
        );

        let waiter2 = cond.wait();
        assert_eq!(
            lazy(|cx| waiter2.poll_ready(cx)).await,
            Poll::Ready("".into())
        );
    }
}
