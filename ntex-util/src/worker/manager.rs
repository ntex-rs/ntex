use std::{fmt, future::Future, io, marker, mem, net, pin::Pin, task::Context, task::Poll};

use async_channel::unbounded;

use crate::rt::{spawn, Signal, System};
use crate::time::{sleep, Millis};
use crate::{io::Io, service::ServiceFactory, util::join_all, util::Stream};

use super::service::{Factory, InternalServiceFactory};
use super::worker::{self, Worker, WorkerAvailability, WorkerClient, WorkerManagerCmd};
use super::{socket, socket::Listener, Server, ServerCommand, ServerStatus, Token};

const STOP_DELAY: Millis = Millis(300);

type ServerStream = Pin<Box<dyn Stream<Item = ServerCommand>>>;

/// Server builder
pub struct ServerBuilder<T> {
    threads: usize,
    workers: Vec<(usize, WorkerClient<socket::Stream>)>,
    exit: bool,
    shutdown_timeout: Millis,
    no_signals: bool,
    cmd: ServerStream,
    manager: Manager,
    notify: Vec<oneshot::Sender<()>>,
}

impl Default for ServerBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> ServerBuilder<T> {
    /// Create new Server builder instance
    pub fn new(factory: T) -> ServerBuilder {
        let (tx, rx) = unbounded();

        ServerBuilder {
            threads: std::thread::available_parallelism()
                .map_or(2, std::num::NonZeroUsize::get),
            workers: Vec::new(),
            exit: false,
            shutdown_timeout: Millis::from_secs(30),
            no_signals: false,
            cmd: Box::pin(rx),
            notify: Vec::new(),
            manager: Manager::new(tx),
        }
    }

    /// Set number of workers to start.
    ///
    /// By default server uses number of available logical cpu as workers
    /// count.
    pub fn workers(mut self, num: usize) -> Self {
        self.threads = num;
        self
    }

    /// Stop ntex runtime when manager get dropped.
    ///
    /// By default "stop runtime" is disabled.
    pub fn stop_runtime(mut self) -> Self {
        self.exit = true;
        self
    }

    /// Disable signal handling.
    ///
    /// By default signal handling is enabled.
    pub fn disable_signals(mut self) -> Self {
        self.no_signals = true;
        self
    }

    /// Timeout for graceful workers shutdown.
    ///
    /// After receiving a stop signal, workers have this much time to finish
    /// serving requests. Workers still alive after the timeout are force
    /// dropped.
    ///
    /// By default shutdown timeout sets to 30 seconds.
    pub fn shutdown_timeout<T: Into<Millis>>(mut self, timeout: T) -> Self {
        self.shutdown_timeout = timeout.into();
        self
    }

    /// Set server status handler.
    ///
    /// Server calls this handler on every inner status update.
    pub fn status_handler<F>(mut self, handler: F) -> Self
    where
        F: FnMut(ManagerStatus) + Send + 'static,
    {
        // self.accept.set_status_handler(handler);
        self
    }

    /// Starts processing incoming connections and return server controller.
    pub fn run(mut self) -> Server {
        if self.sockets.is_empty() {
            panic!("Server should have at least one bound socket");
        } else {
            log::info!("Starting {} workers", self.threads);

            // start workers
            let mut workers = Vec::new();
            for idx in 0..self.threads {
                let worker = self.start_worker(idx, self.accept.notify());
                workers.push(worker.clone());
                self.workers.push((idx, worker));
            }

            // start accept thread
            for sock in &self.sockets {
                log::info!("Starting \"{}\" service on {}", sock.1, sock.2);
            }
            self.accept.start(
                mem::take(&mut self.sockets)
                    .into_iter()
                    .map(|t| (t.0, t.2))
                    .collect(),
                workers,
            );

            // handle signals
            if !self.no_signals {
                spawn(signals(self.server.clone()));
            }

            // start http server actor
            let server = self.server.clone();
            spawn(self);
            server
        }
    }

    fn start_worker(
        &self,
        idx: usize,
        notify: AcceptNotify,
    ) -> WorkerClient<socket::Stream> {
        let avail = WorkerAvailability::new(Box::new(notify));
        let services: Vec<Box<dyn InternalServiceFactory<socket::Stream>>> =
            self.services.iter().map(|v| v.clone_factory()).collect();

        Worker::start(idx, services, avail, self.shutdown_timeout)
    }

    fn handle_cmd(&mut self, item: ServerCommand) {
        match item {
            ServerCommand::Pause(tx) => {
                self.accept.send(WorkerManagerCmd::Pause);
                let _ = tx.send(());
            }
            ServerCommand::Resume(tx) => {
                self.accept.send(WorkerManagerCmd::Resume);
                let _ = tx.send(());
            }
            ServerCommand::Signal(sig) => {
                // Signals support
                // Handle `SIGINT`, `SIGTERM`, `SIGQUIT` signals and stop ntex system
                match sig {
                    Signal::Int => {
                        log::info!("SIGINT received, exiting");
                        self.exit = true;
                        self.handle_cmd(ServerCommand::Stop {
                            graceful: false,
                            completion: None,
                        })
                    }
                    Signal::Term => {
                        log::info!("SIGTERM received, stopping");
                        self.exit = true;
                        self.handle_cmd(ServerCommand::Stop {
                            graceful: true,
                            completion: None,
                        })
                    }
                    Signal::Quit => {
                        log::info!("SIGQUIT received, exiting");
                        self.exit = true;
                        self.handle_cmd(ServerCommand::Stop {
                            graceful: false,
                            completion: None,
                        })
                    }
                    _ => (),
                }
            }
            ServerCommand::Notify(tx) => {
                self.notify.push(tx);
            }
            ServerCommand::Stop {
                graceful,
                completion,
            } => {
                let exit = self.exit;

                // stop accept thread
                let (tx, rx) = std::sync::mpsc::channel();
                self.accept.send(WorkerManagerCmd::Stop(tx));
                let _ = rx.recv();

                let notify = std::mem::take(&mut self.notify);

                // stop workers
                if !self.workers.is_empty() && graceful {
                    let futs: Vec<_> = self
                        .workers
                        .iter()
                        .map(move |worker| worker.1.stop(graceful))
                        .collect();
                    spawn(async move {
                        let _ = join_all(futs).await;

                        if let Some(tx) = completion {
                            let _ = tx.send(());
                        }
                        for tx in notify {
                            let _ = tx.send(());
                        }
                        if exit {
                            sleep(STOP_DELAY).await;
                            System::current().stop();
                        }
                    });
                } else {
                    self.workers.iter().for_each(move |worker| {
                        worker.1.stop(false);
                    });

                    // we need to stop system if server was spawned
                    if self.exit {
                        spawn(async {
                            sleep(STOP_DELAY).await;
                            System::current().stop();
                        });
                    }
                    if let Some(tx) = completion {
                        let _ = tx.send(());
                    }
                    for tx in notify {
                        let _ = tx.send(());
                    }
                }
            }
            ServerCommand::WorkerFaulted(idx) => {
                let mut found = false;
                for i in 0..self.workers.len() {
                    if self.workers[i].0 == idx {
                        self.workers.swap_remove(i);
                        found = true;
                        break;
                    }
                }

                if found {
                    error!("Worker has died {:?}, restarting", idx);

                    let mut new_idx = self.workers.len();
                    'found: loop {
                        for i in 0..self.workers.len() {
                            if self.workers[i].0 == new_idx {
                                new_idx += 1;
                                continue 'found;
                            }
                        }
                        break;
                    }

                    let worker = self.start_worker(new_idx, self.accept.notify());
                    self.workers.push((new_idx, worker.clone()));
                    self.accept.send(WorkerManagerCmd::Worker(worker));
                }
            }
        }
    }
}
