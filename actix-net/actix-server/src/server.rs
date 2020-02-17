use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures::channel::mpsc::UnboundedSender;
use futures::channel::oneshot;
use futures::FutureExt;

use crate::builder::ServerBuilder;
use crate::signals::Signal;

#[derive(Debug)]
pub(crate) enum ServerCommand {
    WorkerFaulted(usize),
    Pause(oneshot::Sender<()>),
    Resume(oneshot::Sender<()>),
    Signal(Signal),
    /// Whether to try and shut down gracefully
    Stop {
        graceful: bool,
        completion: Option<oneshot::Sender<()>>,
    },
    /// Notify of server stop
    Notify(oneshot::Sender<()>),
}

#[derive(Debug)]
pub struct Server(
    UnboundedSender<ServerCommand>,
    Option<oneshot::Receiver<()>>,
);

impl Server {
    pub(crate) fn new(tx: UnboundedSender<ServerCommand>) -> Self {
        Server(tx, None)
    }

    /// Start server building process
    pub fn build() -> ServerBuilder {
        ServerBuilder::default()
    }

    pub(crate) fn signal(&self, sig: Signal) {
        let _ = self.0.unbounded_send(ServerCommand::Signal(sig));
    }

    pub(crate) fn worker_faulted(&self, idx: usize) {
        let _ = self.0.unbounded_send(ServerCommand::WorkerFaulted(idx));
    }

    /// Pause accepting incoming connections
    ///
    /// If socket contains some pending connection, they might be dropped.
    /// All opened connection remains active.
    pub fn pause(&self) -> impl Future<Output = ()> {
        let (tx, rx) = oneshot::channel();
        let _ = self.0.unbounded_send(ServerCommand::Pause(tx));
        rx.map(|_| ())
    }

    /// Resume accepting incoming connections
    pub fn resume(&self) -> impl Future<Output = ()> {
        let (tx, rx) = oneshot::channel();
        let _ = self.0.unbounded_send(ServerCommand::Resume(tx));
        rx.map(|_| ())
    }

    /// Stop incoming connection processing, stop all workers and exit.
    ///
    /// If server starts with `spawn()` method, then spawned thread get terminated.
    pub fn stop(&self, graceful: bool) -> impl Future<Output = ()> {
        let (tx, rx) = oneshot::channel();
        let _ = self.0.unbounded_send(ServerCommand::Stop {
            graceful,
            completion: Some(tx),
        });
        rx.map(|_| ())
    }
}

impl Clone for Server {
    fn clone(&self) -> Self {
        Self(self.0.clone(), None)
    }
}

impl Future for Server {
    type Output = io::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        if this.1.is_none() {
            let (tx, rx) = oneshot::channel();
            if this.0.unbounded_send(ServerCommand::Notify(tx)).is_err() {
                return Poll::Ready(Ok(()));
            }
            this.1 = Some(rx);
        }

        match Pin::new(this.1.as_mut().unwrap()).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(_)) => Poll::Ready(Ok(())),
            Poll::Ready(Err(_)) => Poll::Ready(Ok(())),
        }
    }
}
