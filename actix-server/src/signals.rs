use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use actix_rt::spawn;
use futures::future::LocalBoxFuture;
use futures::stream::{futures_unordered, FuturesUnordered, LocalBoxStream};
use futures::{FutureExt, Stream, StreamExt, TryFutureExt, TryStream, TryStreamExt};
use tokio_net::signal::unix::signal;

use crate::server::Server;

/// Different types of process signals
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
    stream: SigStream,
    #[cfg(unix)]
    streams: Vec<SigStream>,
}

type SigStream = LocalBoxStream<'static, Result<Signal, io::Error>>;

impl Signals {
    pub(crate) fn start(srv: Server) {
        let fut = {
            #[cfg(not(unix))]
            {
                tokio_net::signal::ctrl_c()
                    .map_err(|_| ())
                    .and_then(move |stream| Signals {
                        srv,
                        stream: Box::new(stream.map(|_| Signal::Int)),
                    })
            }

            #[cfg(unix)]
            {
                use tokio_net::signal::unix;

                let mut sigs: Vec<_> = Vec::new();

                let mut SIG_MAP = [
                    (
                        tokio_net::signal::unix::SignalKind::interrupt(),
                        Signal::Int,
                    ),
                    (tokio_net::signal::unix::SignalKind::hangup(), Signal::Hup),
                    (
                        tokio_net::signal::unix::SignalKind::terminate(),
                        Signal::Term,
                    ),
                    (tokio_net::signal::unix::SignalKind::quit(), Signal::Quit),
                ];

                for (kind, sig) in SIG_MAP.into_iter() {
                    let sig = sig.clone();
                    let fut = signal(*kind).unwrap();
                    sigs.push(fut.map(move |_| Ok(sig)).boxed_local());
                }
                /* TODO: Finish rewriting this
                sigs.push(
                    tokio_net::signal::unix::signal(tokio_net::signal::si).unwrap()
                        .map(|stream| {
                            let s: SigStream = Box::new(stream.map(|_| Signal::Int));
                            s
                        }).boxed()
                );
                sigs.push(

                    tokio_net::signal::unix::signal(tokio_net::signal::unix::SignalKind::hangup()).unwrap()
                        .map(|stream: unix::Signal| {
                            let s: SigStream = Box::new(stream.map(|_| Signal::Hup));
                            s
                        }).boxed()
                );
                sigs.push(
                    tokio_net::signal::unix::signal(
                        tokio_net::signal::unix::SignalKind::terminate()
                    ).unwrap()
                        .map(|stream| {
                            let s: SigStream = Box::new(stream.map(|_| Signal::Term));
                            s
                        }).boxed(),
                );
                sigs.push(
                    tokio_net::signal::unix::signal(
                        tokio_net::signal::unix::SignalKind::quit()
                    ).unwrap()
                        .map(|stream| {
                            let s: SigStream = Box::new(stream.map(|_| Signal::Quit));
                            s
                        }).boxed()
                );
                */

                Signals { srv, streams: sigs }
            }
        };
        spawn(async {});
    }
}

impl Future for Signals {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        unimplemented!()
    }

    /*
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        #[cfg(not(unix))]
        loop {
            match self.stream.poll() {
                Ok(Async::Ready(None)) | Err(_) => return Ok(Async::Ready(())),
                Ok(Async::Ready(Some(sig))) => self.srv.signal(sig),
                Ok(Async::NotReady) => return Ok(Async::NotReady),
            }
        }
        #[cfg(unix)]
        {
            for s in &mut self.streams {
                loop {
                    match s.poll() {
                        Ok(Async::Ready(None)) | Err(_) => return Ok(Async::Ready(())),
                        Ok(Async::NotReady) => break,
                        Ok(Async::Ready(Some(sig))) => self.srv.signal(sig),
                    }
                }
            }
            Ok(Async::NotReady)
        }
    }
    */
}
