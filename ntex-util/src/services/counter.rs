use std::{cell::Cell, future::poll_fn, rc::Rc, task};

use crate::task::LocalWaker;

/// Simple counter with ability to notify task on reaching specific number
///
/// Counter could be cloned, total count is shared across all clones.
#[derive(Debug)]
pub struct Counter(Rc<CounterInner>);

#[derive(Debug)]
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
    pub(crate) fn get(&self) -> CounterGuard {
        CounterGuard::new(self.0.clone())
    }

    pub(crate) fn is_available(&self) -> bool {
        self.0.count.get() < self.0.capacity
    }

    /// Check if counter is not at capacity. If counter at capacity
    /// it registers notification for current task.
    pub(crate) async fn available(&self) {
        poll_fn(|cx| {
            if self.poll_available(cx) {
                task::Poll::Ready(())
            } else {
                task::Poll::Pending
            }
        })
        .await
    }

    pub(crate) async fn unavailable(&self) {
        poll_fn(|cx| {
            if self.poll_available(cx) {
                task::Poll::Pending
            } else {
                task::Poll::Ready(())
            }
        })
        .await
    }

    /// Check if counter is not at capacity. If counter at capacity
    /// it registers notification for current task.
    fn poll_available(&self, cx: &mut task::Context<'_>) -> bool {
        self.0.available(cx)
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
        let num = self.count.get() + 1;
        self.count.set(num);
        if num == self.capacity {
            self.task.wake();
        }
    }

    fn dec(&self) {
        let num = self.count.get();
        self.count.set(num - 1);
        if num == self.capacity {
            self.task.wake();
        }
    }

    fn available(&self, cx: &mut task::Context<'_>) -> bool {
        self.task.register(cx.waker());
        if self.count.get() < self.capacity {
            true
        } else {
            false
        }
    }
}
