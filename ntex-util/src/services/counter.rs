use std::{cell::Cell, cell::RefCell, future::poll_fn, rc::Rc, task::Context, task::Poll};

use crate::task::LocalWaker;

/// Simple counter with ability to notify task on reaching specific number
///
/// Counter could be cloned, total count is shared across all clones.
#[derive(Debug)]
pub struct Counter(usize, Rc<CounterInner>);

#[derive(Debug)]
struct CounterInner {
    count: Cell<usize>,
    capacity: Cell<usize>,
    tasks: RefCell<slab::Slab<LocalWaker>>,
}

impl Counter {
    /// Create `Counter` instance and set max value.
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

    /// Get counter guard.
    pub fn get(&self) -> CounterGuard {
        CounterGuard::new(self.1.clone())
    }

    /// Set counter capacity
    pub fn set_capacity(&self, cap: usize) {
        self.1.capacity.set(cap);
        self.1.notify();
    }

    /// Check is counter has free capacity.
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
            if self.poll_available(cx) {
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

    /// Get total number of acquired counts
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
        self.count.set(self.count.get() + 1);
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
