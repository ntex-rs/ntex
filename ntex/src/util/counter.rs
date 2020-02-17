use std::cell::Cell;
use std::rc::Rc;
use std::task;

use crate::task::LocalWaker;

#[derive(Clone)]
/// Simple counter with ability to notify task on reaching specific number
///
/// Counter could be cloned, total ncount is shared across all clones.
pub struct Counter(Rc<CounterInner>);

struct CounterInner {
    count: Cell<usize>,
    capacity: usize,
    task: LocalWaker,
}

impl Counter {
    /// Create `Counter` instance and set max value.
    pub fn new(capacity: usize) -> Self {
        Counter(Rc::new(CounterInner {
            capacity,
            count: Cell::new(0),
            task: LocalWaker::new(),
        }))
    }

    /// Get counter guard.
    pub fn get(&self) -> CounterGuard {
        CounterGuard::new(self.0.clone())
    }

    /// Check if counter is not at capacity. If counter at capacity
    /// it registers notification for current task.
    pub fn available(&self, cx: &mut task::Context<'_>) -> bool {
        self.0.available(cx)
    }

    /// Get total number of acquired counts
    pub fn total(&self) -> usize {
        self.0.count.get()
    }
}

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
        if num == self.capacity {
            self.task.wake();
        }
    }

    fn available(&self, cx: &mut task::Context<'_>) -> bool {
        if self.count.get() < self.capacity {
            true
        } else {
            self.task.register(cx.waker());
            false
        }
    }
}
