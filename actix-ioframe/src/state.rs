use std::cell::{Ref, RefCell, RefMut};
use std::rc::Rc;

/// Connection state
///
/// Connection state is an arbitrary data attached to the each incoming message.
#[derive(Debug)]
pub struct State<T>(Rc<RefCell<T>>);

impl<T> State<T> {
    pub(crate) fn new(st: T) -> Self {
        State(Rc::new(RefCell::new(st)))
    }

    #[inline]
    pub fn get_ref(&self) -> Ref<T> {
        self.0.borrow()
    }

    #[inline]
    pub fn get_mut(&mut self) -> RefMut<T> {
        self.0.borrow_mut()
    }
}

impl<T> Clone for State<T> {
    fn clone(&self) -> Self {
        State(self.0.clone())
    }
}
