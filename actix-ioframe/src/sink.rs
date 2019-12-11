use std::fmt;

use actix_utils::{mpsc, oneshot};
use futures::future::{Future, FutureExt};

use crate::dispatcher::Message;

pub struct Sink<T, E>(mpsc::Sender<Result<Message<T>, E>>);

impl<T, E> Clone for Sink<T, E> {
    fn clone(&self) -> Self {
        Sink(self.0.clone())
    }
}

impl<T, E> Sink<T, E> {
    pub(crate) fn new(tx: mpsc::Sender<Result<Message<T>, E>>) -> Self {
        Sink(tx)
    }

    /// Close connection
    pub fn close(&self) {
        let _ = self.0.send(Ok(Message::Close));
    }

    /// Close connection
    pub fn wait_close(&self) -> impl Future<Output = ()> {
        let (tx, rx) = oneshot::channel();
        let _ = self.0.send(Ok(Message::WaitClose(tx)));

        rx.map(|_| ())
    }

    /// Send item
    pub fn send(&self, item: T) {
        let _ = self.0.send(Ok(Message::Item(item)));
    }
}

impl<T, E> fmt::Debug for Sink<T, E> {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("Sink").finish()
    }
}
