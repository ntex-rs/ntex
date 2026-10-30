use std::{cell, fmt, future::Future, future::poll_fn, pin::Pin, task::Context, task::Poll};

use slab::Slab;

use super::cell::Cell;
use crate::task::LocalWaker;

#[derive(Clone, Debug, PartialEq, Eq)]
/// Result produced by a [`Condition`] waiter.
pub enum ConditionResult<T> {
    /// The condition delivered a value.
    Value(T),
    /// The condition has been locked and will not deliver more values.
    Locked,
    /// The last handle to the condition was dropped.
    Dropped,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum State {
    Normal,
    Locked,
    Dropped,
}

/// A condition that can wake several waiting tasks at once.
///
/// Notifications are not queued. A waiter must have been polled and registered
/// its waker before [`notify`](Self::notify) is called, otherwise it misses that
/// value. Use [`notify_and_lock`](Self::notify_and_lock) when no later
/// notifications should be accepted.
pub struct Condition<T = ()> {
    inner: Cell<Inner<T>>,
}

/// A task waiting for a [`Condition`] notification.
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
    /// Creates an unlocked condition with no waiters.
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
    /// Creates a new waiter.
    ///
    /// The waiter starts listening when it is first polled, not when this
    /// method returns.
    pub fn wait(&self) -> Waiter<T> {
        let token = self.inner.get_mut().data.insert(None);
        Waiter {
            token,
            inner: self.inner.clone(),
        }
    }

    /// Sends `val` to every waiter that is currently being polled.
    ///
    /// The value is cloned for each registered waiter. Unpolled waiters do not
    /// receive it, and the value is not retained for future waiters.
    pub fn notify(&self, val: T) {
        let inner = self.inner.get_ref();
        if inner.state != State::Normal {
            return;
        }
        for (_, item) in &inner.data {
            if let Some(item) = item
                && item.waker.wake_checked()
            {
                item.val.set(ConditionResult::Value(val.clone()));
            }
        }
    }

    /// Notifies the current waiters and permanently locks the condition.
    ///
    /// Registered waiters receive `val`. Later readiness checks return
    /// [`ConditionResult::Locked`], and later calls to [`notify`](Self::notify)
    /// do not deliver another value.
    pub fn notify_and_lock(&self, val: T) {
        self.notify(val);
        self.inner.get_mut().state = State::Locked;
    }
}

impl<T: Default> Condition<T> {
    /// Sends `T::default()` to every waiter that is currently being polled.
    pub fn notify_default(&self) {
        let inner = self.inner.get_ref();
        if inner.state != State::Normal {
            return;
        }
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
    /// Waits for the next condition result.
    pub async fn ready(&self) -> ConditionResult<T> {
        poll_fn(|cx| self.poll_ready(cx)).await
    }

    /// Polls for the next condition result.
    ///
    /// The first poll registers this waiter. While the condition remains
    /// unlocked, later notifications wake the registered task.
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

impl<T> Future for Waiter<T> {
    type Output = ConditionResult<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.get_mut().poll_ready(cx)
    }
}

impl<T> fmt::Debug for Waiter<T> {
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

    /// STATEMENT: Condition waiters receive notifications and observe the condition's drop.
    ///
    /// HOW:
    /// - Poll a waiter pending, notify its default value, and check its result and debug output.
    /// - Drop the condition with two pending cloned waiters and verify both report `Dropped`.
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

    /// STATEMENT: Polling consumes a condition notification but keeps drop results repeatable.
    ///
    /// HOW:
    /// - Notify and await a waiter, then poll it and its clone repeatedly as pending.
    /// - Drop the condition and verify repeated polls of both return `Dropped`.
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

    /// STATEMENT: Conditions deliver supplied values and report drop after the value is consumed.
    ///
    /// HOW:
    /// - Notify a string and verify the waiter receives that value.
    /// - Poll the waiter and its clone before and after dropping the condition.
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

    /// STATEMENT: Awaiting a condition value does not require the value to implement `Default`.
    ///
    /// HOW:
    /// - Create a condition for a cloneable non-default type and poll its waiter pending.
    /// - Notify a concrete value and verify awaiting the waiter returns it.
    #[ntex::test]
    async fn waiter_future_does_not_require_default() {
        #[derive(Clone, Debug, PartialEq, Eq)]
        struct Value(&'static str);

        let cond = Condition::<Value>::new();
        let waiter = cond.wait();
        assert_eq!(lazy(|cx| waiter.poll_ready(cx)).await, Poll::Pending);

        cond.notify(Value("ready"));
        assert_eq!(waiter.await, ConditionResult::Value(Value("ready")));
    }

    /// STATEMENT: Locking a condition delivers its final value once and rejects later notifications.
    ///
    /// HOW:
    /// - Notify and lock a pending waiter, then check its value followed by repeated `Locked` results.
    /// - Notify again and create another waiter, verifying neither escapes the locked state.
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
        cond.notify(());
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

    /// STATEMENT: Notify-and-lock delivers the final value to every existing waiter.
    ///
    /// HOW:
    /// - Register two pending waiters, lock with a string value, and consume it from each.
    /// - Verify subsequent polls and a newly created waiter report `Locked`.
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

    /// STATEMENT: A default notification cannot unlock an already locked condition.
    ///
    /// HOW:
    /// - Lock the condition, create a waiter, and issue a default notification.
    /// - Poll the waiter as a future and verify it still reports `Locked`.
    #[ntex::test]
    async fn notify_default_locked() {
        let cond = Condition::<()>::new();
        cond.notify_and_lock(());
        let mut waiter = cond.wait();
        cond.notify_default();
        assert_eq!(
            lazy(|cx| Pin::new(&mut waiter).poll(cx)).await,
            Poll::Ready(ConditionResult::Locked)
        );
    }
}
