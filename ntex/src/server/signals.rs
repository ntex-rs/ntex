use std::pin::Pin;
use std::task::{Context, Poll};

use futures::future::{Future, FutureExt};
use futures::stream::{unfold, Stream, StreamExt};

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

pub(super) struct Signals {
    srv: Server,
    streams: Vec<(Signal, Pin<Box<dyn Stream<Item = ()>>>)>,
}

impl Signals {
    pub(super) fn new(srv: Server) -> Signals {
        let mut signals = Signals {
            srv,
            streams: vec![(
                Signal::Int,
                unfold((), |_| {
                    crate::rt::signal::ctrl_c().map(|res| match res {
                        Ok(_) => Some(((), ())),
                        Err(_) => None,
                    })
                })
                .boxed_local(),
            )],
        };

        #[cfg(unix)]
        {
            use crate::rt::signal::unix;

            let sig_map = [
                (unix::SignalKind::hangup(), Signal::Hup),
                (unix::SignalKind::terminate(), Signal::Term),
                (unix::SignalKind::quit(), Signal::Quit),
            ];

            for (kind, sig) in sig_map.iter() {
                match unix::signal(*kind) {
                    Ok(stream) => signals.streams.push((*sig, stream.boxed_local())),
                    Err(e) => log::error!(
                        "Can not initialize stream handler for {:?} err: {}",
                        sig,
                        e
                    ),
                }
            }
        }

        signals
    }
}

impl Future for Signals {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        for idx in 0..self.streams.len() {
            loop {
                match Pin::new(&mut self.streams[idx].1).poll_next(cx) {
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

#[cfg(test)]
mod tests {
    use futures::channel::mpsc;
    use futures::future::{lazy, ready};
    use futures::stream::once;

    use super::*;
    use crate::server::ServerCommand;

    #[ntex_rt::test]
    async fn signals() {
        let (tx, mut rx) = mpsc::unbounded();
        let server = Server::new(tx);
        let mut signals = Signals::new(server);

        signals.streams = vec![(Signal::Int, once(ready(())).boxed_local())];
        let _ = lazy(|cx| Pin::new(&mut signals).poll(cx)).await;

        if let Some(ServerCommand::Signal(sig)) = rx.next().await {
            assert_eq!(sig, Signal::Int);
        }
    }
}
