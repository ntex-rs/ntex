use std::{cell, fmt, future::Future, future::poll_fn, pin::Pin, task::Context, task::Poll};

use slab::Slab;

use super::cell::Cell;
use crate::task::LocalWaker;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConditionResult<T> {
    Value(T),
    /// Condition is completed and locked (no new notifications)
    Locked,
    /// Condition object is dropped (no new notifications)
    Dropped,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum State {
    Normal,
    Locked,
    Dropped,
}

/// Condition allows to notify multiple waiters at the same time
pub struct Condition<T = ()> {
    inner: Cell<Inner<T>>,
}

/// Waits for result from condition
pub struct Waiter<T = ()> {
    token: usize,
    inner: Cell<Inner<T>>,
}

struct Inner<T> {
    data: Slab<Option<Item<T>>>,
    count: usize,
    state: State,
}

struct Item<T> {
    val: cell::Cell<ConditionResult<T>>,
    waker: LocalWaker,
}

impl Default for Condition<()> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Clone for Condition<T> {
    fn clone(&self) -> Self {
        let inner = self.inner.clone();
        inner.get_mut().count += 1;
        Self { inner }
    }
}

impl<T> fmt::Debug for Condition<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Condition")
            .field("state", &self.inner.get_ref().state)
            .finish()
    }
}

impl<T> Condition<T> {
    /// Construct new condition instance
    pub fn new() -> Condition<T> {
        Condition {
            inner: Cell::new(Inner {
                data: Slab::new(),
                count: 1,
                state: State::Normal,
            }),
        }
    }
}

impl<T: Clone> Condition<T> {
    /// Get condition waiter
    pub fn wait(&self) -> Waiter<T> {
        let token = self.inner.get_mut().data.insert(None);
        Waiter {
            token,
            inner: self.inner.clone(),
        }
    }

    /// Notify all waiters
    pub fn notify(&self, val: T) {
        let inner = self.inner.get_ref();
        for (_, item) in &inner.data {
            if let Some(item) = item
                && item.waker.wake_checked()
            {
                item.val.set(ConditionResult::Value(val.clone()));
            }
        }
    }

    /// Notify all waiters.
    ///
    /// All subsequent waiter readiness checks always returns `Locked`
    pub fn notify_and_lock(&self, val: T) {
        self.inner.get_mut().state = State::Locked;
        self.notify(val);
    }
}

impl<T: Default> Condition<T> {
    /// Notify all waiters
    pub fn notify_default(&self) {
        let inner = self.inner.get_ref();
        for (_, item) in &inner.data {
            if let Some(item) = item
                && item.waker.wake_checked()
            {
                item.val.set(ConditionResult::Value(T::default()));
            }
        }
    }
}

impl<T> Drop for Condition<T> {
    fn drop(&mut self) {
        let inner = self.inner.get_mut();
        inner.count -= 1;
        if inner.count == 0 {
            inner.state = State::Dropped;
            for (_, item) in &inner.data {
                if let Some(item) = item
                    && item.waker.wake_checked()
                {
                    item.val.set(ConditionResult::Dropped);
                }
            }
        }
    }
}

impl<T> Waiter<T> {
    /// Returns readiness state of the condition.
    pub async fn ready(&self) -> ConditionResult<T> {
        poll_fn(|cx| self.poll_ready(cx)).await
    }

    /// Returns readiness state of the condition.
    pub fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<ConditionResult<T>> {
        let parent = self.inner.get_mut();
        let inner = unsafe { parent.data.get_unchecked_mut(self.token) };

        if inner.is_none() {
            if parent.state == State::Normal {
                let waker = LocalWaker::default();
                waker.register(cx.waker());
                *inner = Some(Item {
                    waker,
                    val: cell::Cell::new(ConditionResult::Locked),
                });
                return Poll::Pending;
            }
        } else {
            let item = inner.as_mut().unwrap();
            if !item.waker.register(cx.waker()) {
                return Poll::Ready(item.val.replace(ConditionResult::Locked));
            }
        }

        match parent.state {
            State::Normal => Poll::Pending,
            State::Locked => Poll::Ready(ConditionResult::Locked),
            State::Dropped => Poll::Ready(ConditionResult::Dropped),
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
    type Output = ConditionResult<T>;

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

    #[ntex::test]
    #[allow(clippy::unit_cmp)]
    async fn test_condition() {
        let cond = Condition::<()>::new();
        let mut waiter = cond.wait();
        assert_eq!(
            lazy(|cx| Pin::new(&mut waiter).poll(cx)).await,
            Poll::Pending
        );
        cond.notify_default();
        assert!(format!("{cond:?}").contains("Condition"));
        assert!(format!("{waiter:?}").contains("Waiter"));
        assert_eq!(waiter.await, ConditionResult::Value(()));

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
        assert_eq!(waiter.await, ConditionResult::Dropped);
        assert_eq!(waiter2.await, ConditionResult::Dropped);
    }

    #[ntex::test]
    async fn test_condition_poll() {
        let cond = Condition::default().clone();
        let waiter = cond.wait();
        assert_eq!(lazy(|cx| waiter.poll_ready(cx)).await, Poll::Pending);
        cond.notify_default();
        waiter.ready().await;

        let waiter2 = waiter.clone();
        assert_eq!(lazy(|cx| waiter.poll_ready(cx)).await, Poll::Pending);
        assert_eq!(lazy(|cx| waiter.poll_ready(cx)).await, Poll::Pending);
        assert_eq!(lazy(|cx| waiter2.poll_ready(cx)).await, Poll::Pending);
        assert_eq!(lazy(|cx| waiter2.poll_ready(cx)).await, Poll::Pending);

        drop(cond);
        assert_eq!(
            lazy(|cx| waiter.poll_ready(cx)).await,
            Poll::Ready(ConditionResult::Dropped)
        );
        assert_eq!(
            lazy(|cx| waiter.poll_ready(cx)).await,
            Poll::Ready(ConditionResult::Dropped)
        );
        assert_eq!(
            lazy(|cx| waiter2.poll_ready(cx)).await,
            Poll::Ready(ConditionResult::Dropped)
        );
        assert_eq!(
            lazy(|cx| waiter2.poll_ready(cx)).await,
            Poll::Ready(ConditionResult::Dropped)
        );
    }

    #[ntex::test]
    async fn test_condition_with() {
        let cond = Condition::<String>::new();
        let waiter = cond.wait();
        assert_eq!(lazy(|cx| waiter.poll_ready(cx)).await, Poll::Pending);
        cond.notify("TEST".into());
        assert_eq!(
            waiter.ready().await,
            ConditionResult::Value("TEST".to_string())
        );

        let waiter2 = waiter.clone();
        assert_eq!(lazy(|cx| waiter.poll_ready(cx)).await, Poll::Pending);
        assert_eq!(lazy(|cx| waiter.poll_ready(cx)).await, Poll::Pending);
        assert_eq!(lazy(|cx| waiter2.poll_ready(cx)).await, Poll::Pending);
        assert_eq!(lazy(|cx| waiter2.poll_ready(cx)).await, Poll::Pending);

        drop(cond);
        assert_eq!(
            lazy(|cx| waiter.poll_ready(cx)).await,
            Poll::Ready(ConditionResult::Dropped)
        );
        assert_eq!(
            lazy(|cx| waiter.poll_ready(cx)).await,
            Poll::Ready(ConditionResult::Dropped)
        );
        assert_eq!(
            lazy(|cx| waiter2.poll_ready(cx)).await,
            Poll::Ready(ConditionResult::Dropped)
        );
        assert_eq!(
            lazy(|cx| waiter2.poll_ready(cx)).await,
            Poll::Ready(ConditionResult::Dropped)
        );
    }

    #[ntex::test]
    async fn notify_ready() {
        let cond = Condition::default().clone();
        let waiter = cond.wait();
        assert_eq!(lazy(|cx| waiter.poll_ready(cx)).await, Poll::Pending);

        cond.notify_and_lock(());
        assert_eq!(
            lazy(|cx| waiter.poll_ready(cx)).await,
            Poll::Ready(ConditionResult::Value(()))
        );
        assert_eq!(
            lazy(|cx| waiter.poll_ready(cx)).await,
            Poll::Ready(ConditionResult::Locked)
        );
        assert_eq!(
            lazy(|cx| waiter.poll_ready(cx)).await,
            Poll::Ready(ConditionResult::Locked)
        );

        let waiter2 = cond.wait();
        assert_eq!(
            lazy(|cx| waiter2.poll_ready(cx)).await,
            Poll::Ready(ConditionResult::Locked)
        );
    }

    #[ntex::test]
    async fn notify_with_and_lock_ready() {
        // with
        let cond = Condition::<String>::new();
        let waiter = cond.wait();
        let waiter2 = cond.wait();
        assert_eq!(lazy(|cx| waiter.poll_ready(cx)).await, Poll::Pending);
        assert_eq!(lazy(|cx| waiter2.poll_ready(cx)).await, Poll::Pending);

        cond.notify_and_lock("TEST".into());
        assert_eq!(
            lazy(|cx| waiter.poll_ready(cx)).await,
            Poll::Ready(ConditionResult::Value("TEST".into()))
        );
        assert_eq!(
            lazy(|cx| waiter.poll_ready(cx)).await,
            Poll::Ready(ConditionResult::Locked)
        );
        assert_eq!(
            lazy(|cx| waiter.poll_ready(cx)).await,
            Poll::Ready(ConditionResult::Locked)
        );
        assert_eq!(
            lazy(|cx| waiter2.poll_ready(cx)).await,
            Poll::Ready(ConditionResult::Value("TEST".into()))
        );
        assert_eq!(
            lazy(|cx| waiter2.poll_ready(cx)).await,
            Poll::Ready(ConditionResult::Locked)
        );

        let waiter2 = cond.wait();
        assert_eq!(
            lazy(|cx| waiter2.poll_ready(cx)).await,
            Poll::Ready(ConditionResult::Locked)
        );
    }
}
