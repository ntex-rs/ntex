//! Custom cell impl

use std::cell::UnsafeCell;
use std::fmt;
use std::rc::Rc;

pub(crate) struct Cell<T> {
    pub(crate) inner: Rc<UnsafeCell<T>>,
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
    pub(crate) fn new(inner: T) -> Self {
        Self {
            inner: Rc::new(UnsafeCell::new(inner)),
        }
    }

    pub(crate) fn strong_count(&self) -> usize {
        Rc::strong_count(&self.inner)
    }

    pub(crate) fn get_ref(&self) -> &T {
        unsafe { &*self.inner.as_ref().get() }
    }

    pub(crate) fn get_mut(&mut self) -> &mut T {
        unsafe { &mut *self.inner.as_ref().get() }
    }

    #[allow(clippy::mut_from_ref)]
    pub(crate) unsafe fn get_mut_unsafe(&self) -> &mut T {
        &mut *self.inner.as_ref().get()
    }
}
