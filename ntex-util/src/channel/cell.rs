//! Custom cell impl
use std::{cell::UnsafeCell, fmt, rc::Rc, rc::Weak};

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

    pub(super) fn downgrade(&self) -> WeakCell<T> {
        WeakCell {
            inner: Rc::downgrade(&self.inner),
        }
    }
}

#[derive(Debug)]
pub(super) struct WeakCell<T> {
    inner: Weak<UnsafeCell<T>>,
}

impl<T> Clone for WeakCell<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T> WeakCell<T> {
    pub(super) fn upgrade(&self) -> Option<Cell<T>> {
        self.inner.upgrade().map(|inner| Cell { inner })
    }
}
