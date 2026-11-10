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

/// Delivers a result from `make` to every registered waiter.
///
/// `make`, value drops and waker callbacks may reenter the condition, adding
/// or removing waiters and reallocating the slab, so they run without a
/// borrow of `inner`. Waiters added during delivery may be skipped.
fn deliver<T>(inner: &Cell<Inner<T>>, mut make: impl FnMut() -> ConditionResult<T>) {
    let end = inner.get_ref().data.capacity();
    for key in 0..end {
        let registered = matches!(
            inner.get_ref().data.get(key),
            Some(Some(item)) if item.waker.is_set()
        );
        if !registered {
            continue;
        }

        let val = make();
        let delivered = match inner.get_ref().data.get(key) {
            Some(Some(item)) => match item.waker.take_local() {
                Some(waker) => Ok((waker, item.val.replace(val))),
                None => Err(val),
            },
            _ => Err(val),
        };
        if let Ok((waker, prev)) = delivered {
            drop(prev);
            waker.wake();
        }
    }
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
        if self.inner.get_ref().state == State::Normal {
            deliver(&self.inner, || ConditionResult::Value(val.clone()));
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
        if self.inner.get_ref().state == State::Normal {
            deliver(&self.inner, || ConditionResult::Value(T::default()));
        }
    }
}

impl<T> Drop for Condition<T> {
    fn drop(&mut self) {
        let dropped = {
            let inner = self.inner.get_mut();
            inner.count -= 1;
            if inner.count == 0 {
                inner.state = State::Dropped;
            }
            inner.count == 0
        };
        if dropped {
            deliver(&self.inner, || ConditionResult::Dropped);
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
        let new = {
            let parent = self.inner.get_mut();
            // SAFETY: This waiter owns its occupied slot until drop, and no callback runs during this borrow.
            let slot = unsafe { parent.data.get_unchecked_mut(self.token) };
            if slot.is_none() {
                if parent.state != State::Normal {
                    return self.poll_state();
                }
                *slot = Some(Item {
                    waker: LocalWaker::default(),
                    val: cell::Cell::new(ConditionResult::Locked),
                });
                true
            } else {
                false
            }
        };

        // waker callbacks may reenter the condition, so the item is looked up
        // again after each one
        let registered = LocalWaker::register_in(|| &self.item().waker, cx.waker());

        // an unset waker on a registered item means a result was delivered
        if new || registered {
            self.poll_state()
        } else {
            Poll::Ready(self.item().val.replace(ConditionResult::Locked))
        }
    }

    fn item(&self) -> &Item<T> {
        // SAFETY: This waiter's slot stays occupied until drop; `poll_ready` initializes its item before access.
        unsafe { self.inner.get_ref().data.get_unchecked(self.token) }
            .as_ref()
            .unwrap()
    }

    fn poll_state(&self) -> Poll<ConditionResult<T>> {
        match self.inner.get_ref().state {
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
        let item = self.inner.get_mut().data.remove(self.token);
        drop(item);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::future::lazy;
    use crate::task::hooks::{ON_CLONE, ON_WAKE, hook_waker};

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

    /// STATEMENT: Waiters stored in the shared slab observe values, locking and the
    /// condition's drop without undefined behavior.
    ///
    /// HOW:
    /// - Poll two waiters pending with `Waker::noop`, notify `1`, and check both get it.
    /// - Lock a cloned condition with `2`, check the waiter returns it, then `Locked`.
    /// - Check a new waiter of the locked condition is immediately `Locked`.
    /// - Drop both condition handles and check the remaining waiter reports `Dropped`.
    #[test]
    fn miri_waiters_follow_notify_lock_and_drop() {
        let mut cx = Context::from_waker(std::task::Waker::noop());
        let cond = Condition::<String>::new();
        let w1 = cond.wait();
        let w2 = w1.clone();
        assert!(w1.poll_ready(&mut cx).is_pending());
        assert!(w2.poll_ready(&mut cx).is_pending());

        cond.notify("1".to_string());
        let one = Poll::Ready(ConditionResult::Value("1".to_string()));
        assert_eq!(w1.poll_ready(&mut cx), one);
        assert_eq!(w2.poll_ready(&mut cx), one);
        assert!(w1.poll_ready(&mut cx).is_pending());

        let cond2 = cond.clone();
        cond2.notify_and_lock("2".to_string());
        assert!(format!("{cond2:?}").contains("Locked"));
        let two = Poll::Ready(ConditionResult::Value("2".to_string()));
        assert_eq!(w1.poll_ready(&mut cx), two);
        assert_eq!(w1.poll_ready(&mut cx), Poll::Ready(ConditionResult::Locked));
        drop(w1);

        let w3 = cond.wait();
        assert_eq!(w3.poll_ready(&mut cx), Poll::Ready(ConditionResult::Locked));

        assert_eq!(w2.poll_ready(&mut cx), two);
        drop(cond);
        drop(cond2);
        assert_eq!(
            w2.poll_ready(&mut cx),
            Poll::Ready(ConditionResult::Dropped)
        );
    }

    /// STATEMENT: `notify` delivers to a waiter whose wake callback reallocates the waiter slab.
    ///
    /// HOW:
    /// - Register a waiter with `hook_waker`, then set an `ON_WAKE` hook that creates 64 waiters.
    /// - Call `notify` and check the hook ran and the waiter is ready with the value.
    #[test]
    fn miri_notify_with_reentrant_wake() {
        let cond = Condition::<usize>::new();
        let waiter = cond.wait();
        let waker = hook_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(waiter.poll_ready(&mut cx).is_pending());

        let extra = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let (c, e) = (cond.clone(), extra.clone());
        ON_WAKE.set(Some(Box::new(move || {
            e.borrow_mut().extend((0..64).map(|_| c.wait()));
        })));
        cond.notify(1);

        assert_eq!(extra.borrow().len(), 64);
        assert_eq!(
            waiter.poll_ready(&mut cx),
            Poll::Ready(ConditionResult::Value(1))
        );
    }

    /// STATEMENT: The first `poll_ready` registers its waiter even when the waker clone reallocates the waiter slab.
    ///
    /// HOW:
    /// - Set an `ON_CLONE` hook that creates 64 waiters, then poll a new waiter with `hook_waker`.
    /// - Check the poll is pending, then `notify` and check the waiter is ready with the value.
    #[test]
    fn miri_first_poll_with_reentrant_clone() {
        let cond = Condition::<usize>::new();
        let waiter = cond.wait();
        let waker = hook_waker();
        let mut cx = Context::from_waker(&waker);

        let extra = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let (c, e) = (cond.clone(), extra.clone());
        ON_CLONE.set(Some(Box::new(move || {
            e.borrow_mut().extend((0..64).map(|_| c.wait()));
        })));
        assert!(waiter.poll_ready(&mut cx).is_pending());
        assert_eq!(extra.borrow().len(), 64);

        cond.notify(1);
        assert_eq!(
            waiter.poll_ready(&mut cx),
            Poll::Ready(ConditionResult::Value(1))
        );
    }
}
