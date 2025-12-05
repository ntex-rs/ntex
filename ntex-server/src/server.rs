use std::sync::{Arc, atomic::AtomicBool, atomic::Ordering};
use std::task::{Context, Poll, ready};
use std::{future::Future, io, pin::Pin};

use async_channel::Sender;

use crate::{manager::ServerCommand, signals::Signal};

#[derive(Debug)]
pub(crate) struct ServerShared {
    pub(crate) paused: AtomicBool,
}

/// Server controller
#[derive(Debug)]
pub struct Server<T> {
    shared: Arc<ServerShared>,
    cmd: Sender<ServerCommand<T>>,
    stop: Option<oneshot::Receiver<()>>,
}

impl<T> Server<T> {
    pub(crate) fn new(cmd: Sender<ServerCommand<T>>, shared: Arc<ServerShared>) -> Self {
        Server {
            cmd,
            shared,
            stop: None,
        }
    }

    /// Start streaming server building process
    pub fn build() -> crate::net::ServerBuilder {
        crate::net::ServerBuilder::default()
    }

    pub(crate) fn signal(&self, sig: Signal) {
        let _ = self.cmd.try_send(ServerCommand::Signal(sig));
    }

    /// Send item to worker pool
    pub fn process(&self, item: T) -> Result<(), T> {
        if self.shared.paused.load(Ordering::Acquire) {
            Err(item)
        } else if let Err(e) = self.cmd.try_send(ServerCommand::Item(item)) {
            match e.into_inner() {
                ServerCommand::Item(item) => Err(item),
                _ => panic!(),
            }
        } else {
            Ok(())
        }
    }

    /// Pause accepting incoming connections
    ///
    /// If socket contains some pending connection, they might be dropped.
    /// All opened connection remains active.
    pub fn pause(&self) -> impl Future<Output = ()> + use<T> {
        let (tx, rx) = oneshot::channel();
        let _ = self.cmd.try_send(ServerCommand::Pause(tx));
        async move {
            let _ = rx.await;
        }
    }

    /// Resume accepting incoming connections
    pub fn resume(&self) -> impl Future<Output = ()> + use<T> {
        let (tx, rx) = oneshot::channel();
        let _ = self.cmd.try_send(ServerCommand::Resume(tx));
        async move {
            let _ = rx.await;
        }
    }

    /// Stop incoming connection processing, stop all workers and exit.
    ///
    /// If server starts with `spawn()` method, then spawned thread get terminated.
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
            let (tx, rx) = oneshot::channel();
            if this.cmd.try_send(ServerCommand::NotifyStopped(tx)).is_err() {
                return Poll::Ready(Ok(()));
            }
            this.stop = Some(rx);
        }

        let _ = ready!(Pin::new(this.stop.as_mut().unwrap()).poll(cx));

        Poll::Ready(Ok(()))
    }
}
