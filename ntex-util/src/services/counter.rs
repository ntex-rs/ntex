use std::{cell::Cell, cell::RefCell, future::poll_fn, rc::Rc, task::Context, task::Poll};

use crate::task::LocalWaker;

/// A shared count with an asynchronous capacity notification.
///
/// Clones use the same count and capacity, but each clone registers its own
/// waiting task.
#[derive(Debug)]
pub struct Counter(usize, Rc<CounterInner>);

#[derive(Debug)]
struct CounterInner {
    count: Cell<usize>,
    capacity: Cell<usize>,
    tasks: RefCell<slab::Slab<LocalWaker>>,
}

impl Counter {
    /// Creates a counter with the specified capacity.
    pub fn new(capacity: usize) -> Self {
        let mut tasks = slab::Slab::new();
        let idx = tasks.insert(LocalWaker::new());

        Counter(
            idx,
            Rc::new(CounterInner {
                count: Cell::new(0),
                capacity: Cell::new(capacity),
                tasks: RefCell::new(tasks),
            }),
        )
    }

    /// Acquires one count and returns a guard that releases it on drop.
    ///
    /// This does not wait for capacity; call [`available`](Self::available)
    /// first when exceeding the configured capacity is not acceptable.
    pub fn get(&self) -> CounterGuard {
        CounterGuard::new(self.1.clone())
    }

    /// Changes the capacity and wakes waiting tasks.
    pub fn set_capacity(&self, cap: usize) {
        self.1.capacity.set(cap);
        self.1.notify();
    }

    /// Returns `true` if another count can be acquired without exceeding the capacity.
    pub fn is_available(&self) -> bool {
        self.1.count.get() < self.1.capacity.get()
    }

    /// Waits until the counter has free capacity.
    ///
    /// Returns immediately if there is capacity available. Otherwise,
    /// registers the current task for wakeup and waits until a slot is freed.
    pub async fn available(&self) {
        poll_fn(|cx| {
            if self.poll_available(cx) {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
    }

    /// Waits until the counter reaches its capacity (i.e., becomes unavailable).
    pub async fn unavailable(&self) {
        poll_fn(|cx| {
            if self.is_available() {
                self.1.tasks.borrow()[self.0].register(cx.waker());
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        })
        .await;
    }

    /// Check if counter is not at capacity. If counter at capacity
    /// it registers notification for current task.
    fn poll_available(&self, cx: &mut Context<'_>) -> bool {
        if self.1.count.get() < self.1.capacity.get() {
            true
        } else {
            let tasks = self.1.tasks.borrow();
            tasks[self.0].register(cx.waker());
            false
        }
    }

    /// Returns the number of currently held guards.
    pub fn total(&self) -> usize {
        self.1.count.get()
    }
}

impl Clone for Counter {
    fn clone(&self) -> Self {
        let idx = self.1.tasks.borrow_mut().insert(LocalWaker::new());
        Self(idx, self.1.clone())
    }
}

impl Drop for Counter {
    fn drop(&mut self) {
        self.1.tasks.borrow_mut().remove(self.0);
    }
}

#[derive(Debug)]
/// An acquired counter slot.
///
/// Dropping the guard releases the slot and wakes availability waiters.
pub struct CounterGuard(Rc<CounterInner>);

impl CounterGuard {
    fn new(inner: Rc<CounterInner>) -> Self {
        inner.inc();
        CounterGuard(inner)
    }
}

impl Unpin for CounterGuard {}

impl Drop for CounterGuard {
    fn drop(&mut self) {
        self.0.dec();
    }
}

impl CounterInner {
    fn inc(&self) {
        let num = self.count.get() + 1;
        self.count.set(num);
        if num == self.capacity.get() {
            self.notify();
        }
    }

    fn dec(&self) {
        let num = self.count.get();
        self.count.set(num - 1);
        if num == self.capacity.get() {
            self.notify();
        }
    }

    fn notify(&self) {
        let tasks = self.tasks.borrow();
        for (_, task) in &*tasks {
            task.wake();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::time::sleep;

    #[ntex::test]
    async fn test_unavailable_is_woken() {
        let counter = Counter::new(2);
        let done = Rc::new(Cell::new(false));

        let (c, d) = (counter.clone(), done.clone());
        crate::spawn(async move {
            c.unavailable().await;
            d.set(true);
        });
        sleep(Duration::from_millis(10)).await;
        assert!(!done.get());

        let _g1 = counter.get();
        sleep(Duration::from_millis(10)).await;
        assert!(!done.get());

        let _g2 = counter.get();
        sleep(Duration::from_millis(10)).await;
        assert!(done.get());
    }

    #[ntex::test]
    async fn test_available_is_woken() {
        let counter = Counter::new(1);
        let guard = counter.get();
        let done = Rc::new(Cell::new(false));

        let (c, d) = (counter.clone(), done.clone());
        crate::spawn(async move {
            c.available().await;
            d.set(true);
        });
        sleep(Duration::from_millis(10)).await;
        assert!(!done.get());

        drop(guard);
        sleep(Duration::from_millis(10)).await;
        assert!(done.get());
    }

    #[ntex::test]
    async fn test_set_capacity() {
        let counter = Counter::new(1);
        let guard = counter.get();
        assert_eq!(counter.total(), 1);
        assert!(!counter.is_available());

        let counter2 = counter.clone();
        let hnd = crate::spawn(async move { counter2.available().await });
        sleep(Duration::from_millis(10)).await;
        assert!(!hnd.is_finished());

        // raising the capacity wakes waiters
        counter.set_capacity(2);
        assert!(counter.is_available());
        crate::time::timeout(Duration::from_secs(1), hnd)
            .await
            .unwrap()
            .unwrap();

        drop(guard);
        assert_eq!(counter.total(), 0);
    }
}
