#![allow(clippy::missing_panics_doc)]
use std::sync::{Arc, atomic::AtomicU64, atomic::Ordering};
use std::task::{Context, Poll, ready};
use std::{future::Future, io, pin::Pin};

use async_channel::Sender;
use event_listener::Event;
use ntex_rt::signals::Signal;

use crate::manager::{Rejected, Routing, ServerCommand};
use crate::{ServerStatus, metrics::Metrics};

#[derive(Debug)]
pub(crate) struct ServerShared {
    /// Change counter in the high bits, [`ServerStatus`] in the low byte.
    status: AtomicU64,
    status_event: Event,
    pub(crate) metrics: Metrics,
}

impl ServerShared {
    pub(crate) fn new(metrics: Metrics) -> Self {
        ServerShared {
            status: AtomicU64::new(u64::from(ServerStatus::Starting.as_u8())),
            status_event: Event::new(),
            metrics,
        }
    }

    pub(crate) fn status(&self) -> ServerStatus {
        status(self.status.load(Ordering::Acquire))
    }

    /// Sets the status and wakes the waiters if the status has changed.
    ///
    /// Only the server's command loop changes the status.
    pub(crate) fn set_status(&self, st: ServerStatus) {
        let cur = self.status.load(Ordering::Acquire);
        if status(cur) != st {
            let next = ((cur >> 8).wrapping_add(1) << 8) | u64::from(st.as_u8());
            self.status.store(next, Ordering::Release);
            self.status_event.notify(usize::MAX);
        }
    }
}

fn status(val: u64) -> ServerStatus {
    ServerStatus::from_u8(val.to_le_bytes()[0])
}

/// Controller and completion future for a running server.
///
/// Clones can pause, resume, stop, or submit items to the server. Awaiting a
/// `Server` resolves when the server has stopped. It always resolves to
/// `Ok(())`.
#[derive(Debug)]
pub struct Server<T> {
    shared: Arc<ServerShared>,
    router: Arc<Routing<T>>,
    cmd: Sender<ServerCommand<T>>,
    stop: Option<oneshot::AsyncReceiver<()>>,
}

impl Server<crate::net::Connection> {
    /// Creates a network server builder with no application configuration.
    pub fn builder() -> crate::net::ServerBuilder {
        crate::net::ServerBuilder::default()
    }
}

impl<T> Server<T> {
    pub(crate) fn new(
        cmd: Sender<ServerCommand<T>>,
        shared: Arc<ServerShared>,
        router: Arc<Routing<T>>,
    ) -> Self {
        Server {
            cmd,
            shared,
            router,
            stop: None,
        }
    }

    pub(crate) fn metrics_ref(&self) -> &Metrics {
        &self.shared.metrics
    }

    /// Returns a snapshot of the server metrics.
    pub fn metrics(&self) -> crate::ServerMetrics {
        self.shared.metrics.snapshot()
    }

    pub(crate) fn signal(&self, sig: Signal) {
        let _ = self.cmd.try_send(ServerCommand::Signal(sig));
    }

    /// Submits an item to the worker pool.
    ///
    /// The worker is chosen on the calling thread, see
    /// [`LoadBalancing`](crate::LoadBalancing).
    ///
    /// Returns the item unchanged if the server status is not
    /// [`Ready`](ServerStatus::Ready) or the server cannot accept it, so
    /// items submitted right after start or while no worker is available are
    /// rejected.
    pub fn process(&self, item: T) -> Result<(), T> {
        if self.shared.status() != ServerStatus::Ready {
            return Err(item);
        }
        let item = match self.router.dispatch(item, &self.shared.metrics) {
            Ok(()) => return Ok(()),
            // the server started stopping after the status check
            Err(Rejected::Closed(item)) => return Err(item),
            Err(Rejected::NoWorker(item)) => item,
        };
        // no worker is available, the command loop queues the item
        if let Err(e) = self.cmd.try_send(ServerCommand::Item(item)) {
            if let ServerCommand::Item(item) = e.into_inner() {
                Err(item)
            } else {
                panic!()
            }
        } else {
            Ok(())
        }
    }

    /// Returns the current server status.
    pub fn status(&self) -> ServerStatus {
        self.shared.status()
    }

    /// Waits for the server status to change and returns the new status.
    ///
    /// The status is compared with the status at the time of this call, so
    /// changes made before the future is polled are not missed. Changes in
    /// quick succession may be reported once, with the latest status.
    ///
    /// Resolves immediately with [`Stopped`](ServerStatus::Stopped) if the
    /// server has stopped.
    pub fn status_changed(&self) -> impl Future<Output = ServerStatus> + use<T> {
        let shared = self.shared.clone();
        let start = shared.status.load(Ordering::Acquire);
        async move {
            loop {
                let cur = shared.status.load(Ordering::Acquire);
                if cur != start || status(cur) == ServerStatus::Stopped {
                    return status(cur);
                }
                let listener = shared.status_event.listen();
                let cur = shared.status.load(Ordering::Acquire);
                if cur != start {
                    return status(cur);
                }
                listener.await;
            }
        }
    }

    /// Pauses processing new items.
    ///
    /// For network servers, listeners stop accepting connections, so new
    /// connections wait in the kernel listen backlog. Existing connections
    /// remain active.
    ///
    /// The server stays [`Paused`](ServerStatus::Paused) until
    /// [`resume`](Self::resume) is called.
    pub fn pause(&self) -> impl Future<Output = ()> + use<T> {
        let (tx, rx) = oneshot::channel();
        let _ = self.cmd.try_send(ServerCommand::Pause(tx));
        async move {
            let _ = rx.await;
        }
    }

    /// Resumes processing new items.
    ///
    /// The server becomes [`Ready`](ServerStatus::Ready) once a worker is
    /// available.
    pub fn resume(&self) -> impl Future<Output = ()> + use<T> {
        let (tx, rx) = oneshot::channel();
        let _ = self.cmd.try_send(ServerCommand::Resume(tx));
        async move {
            let _ = rx.await;
        }
    }

    /// Stops processing new items and shuts down all workers.
    ///
    /// If `graceful` is `true`, workers are given up to the graceful shutdown
    /// timeout to finish active work before they are stopped. A zero timeout
    /// makes the stop non-graceful.
    ///
    /// The returned future resolves once the stop has completed. If the
    /// server has already stopped, it resolves immediately.
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
            router: self.router.clone(),
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
