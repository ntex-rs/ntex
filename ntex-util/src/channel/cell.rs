use std::{cell::UnsafeCell, fmt, rc::Rc};

/// Shared channel state without runtime borrow tracking.
///
/// References returned by `get_ref` and `get_mut` must not be live while user
/// code runs, such as waker callbacks or `T::clone` and `T::drop`, because
/// that code may reenter the channel. Take wakers and values out first and
/// release the borrow before calling them.
pub(super) struct Cell<T> {
    inner: Rc<UnsafeCell<T>>,
}

impl<T> Clone for Cell<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T: fmt::Debug> fmt::Debug for Cell<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(f)
    }
}

impl<T> Cell<T> {
    pub(super) fn new(inner: T) -> Self {
        Self {
            inner: Rc::new(UnsafeCell::new(inner)),
        }
    }

    pub(super) fn strong_count(&self) -> usize {
        Rc::strong_count(&self.inner)
    }

    pub(super) fn get_ref(&self) -> &T {
        unsafe { &*self.inner.as_ref().get() }
    }

    #[allow(clippy::mut_from_ref)]
    pub(super) fn get_mut(&self) -> &mut T {
        unsafe { &mut *self.inner.as_ref().get() }
    }
}

/// Wakes a waker taken out of channel state, after the borrow has ended.
pub(super) fn wake(waker: Option<ntex_rt::LocalWaker>) {
    if let Some(waker) = waker {
        waker.wake();
    }
}
