//! Custom cell impl

use std::cell::UnsafeCell;
use std::fmt;
use std::rc::{Rc, Weak};

pub(crate) struct Cell<T> {
    pub(crate) inner: Rc<UnsafeCell<T>>,
}

pub(crate) struct WeakCell<T> {
    inner: Weak<UnsafeCell<T>>,
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
    pub fn new(inner: T) -> Self {
        Self {
            inner: Rc::new(UnsafeCell::new(inner)),
        }
    }

    pub fn downgrade(&self) -> WeakCell<T> {
        WeakCell {
            inner: Rc::downgrade(&self.inner),
        }
    }

    pub fn get_ref(&self) -> &T {
        unsafe { &*self.inner.as_ref().get() }
    }

    pub fn get_mut(&mut self) -> &mut T {
        unsafe { &mut *self.inner.as_ref().get() }
    }
}

impl<T> WeakCell<T> {
    pub fn upgrade(&self) -> Option<Cell<T>> {
        if let Some(inner) = self.inner.upgrade() {
            Some(Cell { inner })
        } else {
            None
        }
    }
}

impl<T: fmt::Debug> fmt::Debug for WeakCell<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(f)
    }
}
