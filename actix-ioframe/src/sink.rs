use std::fmt;

use actix_utils::{mpsc, oneshot};
use futures::future::{Future, FutureExt};

use crate::dispatcher::FramedMessage;

pub struct Sink<T>(mpsc::Sender<FramedMessage<T>>);

impl<T> Clone for Sink<T> {
    fn clone(&self) -> Self {
        Sink(self.0.clone())
    }
}

impl<T> Sink<T> {
    pub(crate) fn new(tx: mpsc::Sender<FramedMessage<T>>) -> Self {
        Sink(tx)
    }

    /// Close connection
    pub fn close(&self) {
        let _ = self.0.send(FramedMessage::Close);
    }

    /// Close connection
    pub fn wait_close(&self) -> impl Future<Output = ()> {
        let (tx, rx) = oneshot::channel();
        let _ = self.0.send(FramedMessage::WaitClose(tx));

        rx.map(|_| ())
    }

    /// Send item
    pub fn send(&self, item: T) {
        let _ = self.0.send(FramedMessage::Message(item));
    }
}

impl<T> fmt::Debug for Sink<T> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_struct("Sink").finish()
    }
}
