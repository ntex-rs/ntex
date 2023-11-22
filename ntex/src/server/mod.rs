//! General purpose tcp server
use std::{future::Future, io, pin::Pin, task::Context, task::Poll};

use async_channel::Sender;

mod accept;
mod builder;
mod config;
mod counter;
mod service;
mod socket;
mod test;
mod worker;

#[cfg(feature = "openssl")]
pub use ntex_tls::openssl;

#[cfg(feature = "rustls")]
pub use ntex_tls::rustls;

pub use ntex_tls::max_concurrent_ssl_accept;

pub(crate) use self::builder::create_tcp_listener;
pub use self::builder::ServerBuilder;
pub use self::config::{Config, ServiceConfig, ServiceRuntime};
pub use self::test::{build_test_server, test_server, TestServer};

#[non_exhaustive]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
/// Server readiness status
pub enum ServerStatus {
    Ready,
    NotReady,
    WorkerFailed,
}

/// Socket id token
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Token(usize);

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
    Signal(crate::rt::Signal),
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
pub struct Server(Sender<ServerCommand>, Option<oneshot::Receiver<()>>);

impl Server {
    fn new(tx: Sender<ServerCommand>) -> Self {
        Server(tx, None)
    }

    /// Start server building process
    pub fn build() -> ServerBuilder {
        ServerBuilder::default()
    }

    fn signal(&self, sig: crate::rt::Signal) {
        let _ = self.0.try_send(ServerCommand::Signal(sig));
    }

    fn worker_faulted(&self, idx: usize) {
        let _ = self.0.try_send(ServerCommand::WorkerFaulted(idx));
    }

    /// Pause accepting incoming connections
    ///
    /// If socket contains some pending connection, they might be dropped.
    /// All opened connection remains active.
    pub fn pause(&self) -> impl Future<Output = ()> {
        let (tx, rx) = oneshot::channel();
        let _ = self.0.try_send(ServerCommand::Pause(tx));
        async move {
            let _ = rx.await;
        }
    }

    /// Resume accepting incoming connections
    pub fn resume(&self) -> impl Future<Output = ()> {
        let (tx, rx) = oneshot::channel();
        let _ = self.0.try_send(ServerCommand::Resume(tx));
        async move {
            let _ = rx.await;
        }
    }

    /// Stop incoming connection processing, stop all workers and exit.
    ///
    /// If server starts with `spawn()` method, then spawned thread get terminated.
    pub fn stop(&self, graceful: bool) -> impl Future<Output = ()> {
        let (tx, rx) = oneshot::channel();
        let _ = self.0.try_send(ServerCommand::Stop {
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
            if this.0.try_send(ServerCommand::Notify(tx)).is_err() {
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
