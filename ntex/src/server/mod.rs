//! General purpose tcp server
#![allow(clippy::type_complexity)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::{future::Future, io, pin::Pin};

use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

use crate::util::counter::Counter;

mod accept;
mod builder;
mod config;
mod service;
mod signals;
mod socket;
mod test;
mod worker;

#[cfg(feature = "openssl")]
pub mod openssl;

#[cfg(feature = "rustls")]
pub mod rustls;

pub(crate) use self::builder::create_tcp_listener;
pub use self::builder::ServerBuilder;
pub use self::config::{ServiceConfig, ServiceRuntime};
pub use self::service::StreamServiceFactory;
pub use self::test::{build_test_server, test_server, TestServer};

#[doc(hidden)]
pub use self::socket::FromStream;

/// Socket id token
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(self) struct Token(usize);

impl Token {
    pub(self) fn next(&mut self) -> Token {
        let token = Token(self.0);
        self.0 += 1;
        token
    }
}

/// Start server building process
pub fn build() -> ServerBuilder {
    ServerBuilder::default()
}

/// Sets the maximum per-worker concurrent ssl connection establish process.
///
/// All listeners will stop accepting connections when this limit is
/// reached. It can be used to limit the global SSL CPU usage.
///
/// By default max connections is set to a 256.
pub fn max_concurrent_ssl_accept(num: usize) {
    MAX_SSL_ACCEPT.store(num, Ordering::Relaxed);
}

#[allow(dead_code)]
pub(self) const ZERO: std::time::Duration = std::time::Duration::from_millis(0);
pub(self) static MAX_SSL_ACCEPT: AtomicUsize = AtomicUsize::new(256);

thread_local! {
    static MAX_SSL_ACCEPT_COUNTER: Counter = Counter::new(MAX_SSL_ACCEPT.load(Ordering::Relaxed));
}

/// Ssl error combinded with service error.
#[derive(Debug)]
pub enum SslError<E> {
    Ssl(Box<dyn std::error::Error>),
    Service(E),
}

#[derive(Debug)]
enum ServerCommand {
    WorkerFaulted(usize),
    Pause(oneshot::Sender<()>),
    Resume(oneshot::Sender<()>),
    Signal(signals::Signal),
    /// Whether to try and shut down gracefully
    Stop {
        graceful: bool,
        completion: Option<oneshot::Sender<()>>,
    },
    /// Notify of server stop
    Notify(oneshot::Sender<()>),
}

/// Server controller
#[derive(Debug)]
pub struct Server(
    UnboundedSender<ServerCommand>,
    Option<oneshot::Receiver<()>>,
);

impl Server {
    fn new(tx: UnboundedSender<ServerCommand>) -> Self {
        Server(tx, None)
    }

    /// Start server building process
    pub fn build() -> ServerBuilder {
        ServerBuilder::default()
    }

    fn signal(&self, sig: signals::Signal) {
        let _ = self.0.send(ServerCommand::Signal(sig));
    }

    fn worker_faulted(&self, idx: usize) {
        let _ = self.0.send(ServerCommand::WorkerFaulted(idx));
    }

    /// Pause accepting incoming connections
    ///
    /// If socket contains some pending connection, they might be dropped.
    /// All opened connection remains active.
    pub fn pause(&self) -> impl Future<Output = ()> {
        let (tx, rx) = oneshot::channel();
        let _ = self.0.send(ServerCommand::Pause(tx));
        async move {
            let _ = rx.await;
        }
    }

    /// Resume accepting incoming connections
    pub fn resume(&self) -> impl Future<Output = ()> {
        let (tx, rx) = oneshot::channel();
        let _ = self.0.send(ServerCommand::Resume(tx));
        async move {
            let _ = rx.await;
        }
    }

    /// Stop incoming connection processing, stop all workers and exit.
    ///
    /// If server starts with `spawn()` method, then spawned thread get terminated.
    pub fn stop(&self, graceful: bool) -> impl Future<Output = ()> {
        let (tx, rx) = oneshot::channel();
        let _ = self.0.send(ServerCommand::Stop {
            graceful,
            completion: Some(tx),
        });
        async move {
            let _ = rx.await;
        }
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
            if this.0.send(ServerCommand::Notify(tx)).is_err() {
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
