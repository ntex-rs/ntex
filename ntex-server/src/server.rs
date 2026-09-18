#![allow(clippy::missing_panics_doc)]
use std::sync::{Arc, atomic::AtomicBool, atomic::Ordering};
use std::task::{Context, Poll, ready};
use std::{future::Future, io, pin::Pin};

use async_channel::Sender;
use ntex_rt::signals::Signal;

use crate::manager::ServerCommand;

#[derive(Debug)]
pub(crate) struct ServerShared {
    pub(crate) paused: AtomicBool,
}

/// Controller and completion future for a running server.
///
/// Clones can pause, resume, stop, or submit items to the server. Awaiting a
/// `Server` resolves when the server has stopped.
#[derive(Debug)]
pub struct Server<T> {
    shared: Arc<ServerShared>,
    cmd: Sender<ServerCommand<T>>,
    stop: Option<oneshot::AsyncReceiver<()>>,
}

impl<T> Server<T> {
    pub(crate) fn new(cmd: Sender<ServerCommand<T>>, shared: Arc<ServerShared>) -> Self {
        Server {
            cmd,
            shared,
            stop: None,
        }
    }

    /// Creates a network server builder with no application configuration.
    pub fn builder() -> crate::net::ServerBuilder {
        crate::net::ServerBuilder::default()
    }

    pub(crate) fn signal(&self, sig: Signal) {
        let _ = self.cmd.try_send(ServerCommand::Signal(sig));
    }

    /// Submits an item to the worker pool.
    ///
    /// Returns the item unchanged if the server is paused or cannot accept it.
    pub fn process(&self, item: T) -> Result<(), T> {
        if self.shared.paused.load(Ordering::Acquire) {
            Err(item)
        } else if let Err(e) = self.cmd.try_send(ServerCommand::Item(item)) {
            if let ServerCommand::Item(item) = e.into_inner() {
                Err(item)
            } else {
                panic!()
            }
        } else {
            Ok(())
        }
    }

    /// Pauses processing new items.
    ///
    /// For network servers, pending connections may be dropped. Existing
    /// connections remain active.
    pub fn pause(&self) -> impl Future<Output = ()> + use<T> {
        let (tx, rx) = oneshot::channel();
        let _ = self.cmd.try_send(ServerCommand::Pause(tx));
        async move {
            let _ = rx.await;
        }
    }

    /// Resumes processing new items.
    pub fn resume(&self) -> impl Future<Output = ()> + use<T> {
        let (tx, rx) = oneshot::channel();
        let _ = self.cmd.try_send(ServerCommand::Resume(tx));
        async move {
            let _ = rx.await;
        }
    }

    /// Stops processing new items and shuts down all workers.
    ///
    /// If `graceful` is `true`, workers are given time to finish active work
    /// before they are stopped.
    pub fn stop(&self, graceful: bool) -> impl Future<Output = ()> + use<T> {
        let (tx, rx) = oneshot::channel();
        let _ = self.cmd.try_send(ServerCommand::Stop {
            graceful,
            completion: Some(tx),
        });
        async move {
            let _ = rx.await;
        }
    }
}

impl<T> Clone for Server<T> {
    fn clone(&self) -> Self {
        Self {
            cmd: self.cmd.clone(),
            shared: self.shared.clone(),
            stop: None,
        }
    }
}

impl<T> Future for Server<T> {
    type Output = io::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        if this.stop.is_none() {
            let (tx, rx) = oneshot::async_channel();
            if this.cmd.try_send(ServerCommand::NotifyStopped(tx)).is_err() {
                return Poll::Ready(Ok(()));
            }
            this.stop = Some(rx);
        }

        let _ = ready!(Pin::new(this.stop.as_mut().unwrap()).poll(cx));

        Poll::Ready(Ok(()))
    }
}
