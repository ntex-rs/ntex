use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures::future::lazy;

use crate::server::Server;

/// Different types of process signals
#[allow(dead_code)]
#[derive(PartialEq, Clone, Copy, Debug)]
pub(crate) enum Signal {
    /// SIGHUP
    Hup,
    /// SIGINT
    Int,
    /// SIGTERM
    Term,
    /// SIGQUIT
    Quit,
}

pub(crate) struct Signals {
    srv: Server,
    #[cfg(not(unix))]
    stream: Pin<Box<dyn Future<Output = io::Result<()>>>>,
    #[cfg(unix)]
    streams: Vec<(Signal, actix_rt::signal::unix::Signal)>,
}

impl Signals {
    pub(crate) fn start(srv: Server) -> io::Result<()> {
        actix_rt::spawn(lazy(|_| {
            #[cfg(not(unix))]
            {
                actix_rt::spawn(Signals {
                    srv,
                    stream: Box::pin(actix_rt::signal::ctrl_c()),
                });
            }
            #[cfg(unix)]
            {
                use actix_rt::signal::unix;

                let mut streams = Vec::new();

                let sig_map = [
                    (unix::SignalKind::interrupt(), Signal::Int),
                    (unix::SignalKind::hangup(), Signal::Hup),
                    (unix::SignalKind::terminate(), Signal::Term),
                    (unix::SignalKind::quit(), Signal::Quit),
                ];

                for (kind, sig) in sig_map.iter() {
                    match unix::signal(*kind) {
                        Ok(stream) => streams.push((*sig, stream)),
                        Err(e) => log::error!(
                            "Can not initialize stream handler for {:?} err: {}",
                            sig,
                            e
                        ),
                    }
                }

                actix_rt::spawn(Signals { srv, streams })
            }
        }));

        Ok(())
    }
}

impl Future for Signals {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        #[cfg(not(unix))]
        match Pin::new(&mut self.stream).poll(cx) {
            Poll::Ready(_) => {
                self.srv.signal(Signal::Int);
                Poll::Ready(())
            }
            Poll::Pending => return Poll::Pending,
        }
        #[cfg(unix)]
        {
            for idx in 0..self.streams.len() {
                loop {
                    match self.streams[idx].1.poll_recv(cx) {
                        Poll::Ready(None) => return Poll::Ready(()),
                        Poll::Pending => break,
                        Poll::Ready(Some(_)) => {
                            let sig = self.streams[idx].0;
                            self.srv.signal(sig);
                        }
                    }
                }
            }
            Poll::Pending
        }
    }
}
