use std::fmt;
use std::rc::Rc;

use actix_utils::oneshot;
use futures::future::{Future, FutureExt};

use crate::dispatcher::Message;

pub struct Sink<T>(Rc<dyn Fn(Message<T>)>);

impl<T> Clone for Sink<T> {
    fn clone(&self) -> Self {
        Sink(self.0.clone())
    }
}

impl<T> Sink<T> {
    pub(crate) fn new(tx: Rc<dyn Fn(Message<T>)>) -> Self {
        Sink(tx)
    }

    /// Close connection
    pub fn close(&self) {
        (self.0)(Message::Close);
    }

    /// Close connection
    pub fn wait_close(&self) -> impl Future<Output = ()> {
        let (tx, rx) = oneshot::channel();
        (self.0)(Message::WaitClose(tx));

        rx.map(|_| ())
    }

    /// Send item
    pub fn send(&self, item: T) {
        (self.0)(Message::Item(item));
    }
}

impl<T> fmt::Debug for Sink<T> {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("Sink").finish()
    }
}
