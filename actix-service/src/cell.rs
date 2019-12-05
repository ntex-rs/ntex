//! Custom cell impl, internal use only
use std::task::{Context, Poll};
use std::{cell::UnsafeCell, fmt, rc::Rc};

pub(crate) struct Cell<T> {
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
    pub(crate) fn new(inner: T) -> Self {
        Self {
            inner: Rc::new(UnsafeCell::new(inner)),
        }
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

impl<T: crate::Service> crate::Service for Cell<T> {
    type Request = T::Request;
    type Response = T::Response;
    type Error = T::Error;
    type Future = T::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.get_mut().poll_ready(cx)
    }

    fn call(&mut self, req: Self::Request) -> Self::Future {
        self.get_mut().call(req)
    }
}
