use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use actix_service::{IntoService, Service};
use futures::{FutureExt, Stream};

use crate::mpsc;

#[pin_project::pin_project]
pub struct Dispatcher<S, T>
where
    S: Stream,
    T: Service<Request = S::Item, Response = ()> + 'static,
{
    #[pin]
    stream: S,
    service: T,
    err_rx: mpsc::Receiver<T::Error>,
    err_tx: mpsc::Sender<T::Error>,
}

impl<S, T> Dispatcher<S, T>
where
    S: Stream,
    T: Service<Request = S::Item, Response = ()> + 'static,
{
    pub fn new<F>(stream: S, service: F) -> Self
    where
        F: IntoService<T>,
    {
        let (err_tx, err_rx) = mpsc::channel();
        Dispatcher {
            err_rx,
            err_tx,
            stream,
            service: service.into_service(),
        }
    }
}

impl<S, T> Future for Dispatcher<S, T>
where
    S: Stream,
    T: Service<Request = S::Item, Response = ()> + 'static,
{
    type Output = Result<(), T::Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.as_mut().project();

        if let Poll::Ready(Some(e)) = Pin::new(&mut this.err_rx).poll_next(cx) {
            return Poll::Ready(Err(e));
        }

        loop {
            return match this.service.poll_ready(cx)? {
                Poll::Ready(_) => match this.stream.poll_next(cx) {
                    Poll::Ready(Some(item)) => {
                        let stop = this.err_tx.clone();
                        actix_rt::spawn(this.service.call(item).map(move |res| {
                            if let Err(e) = res {
                                let _ = stop.send(e);
                            }
                        }));
                        this = self.as_mut().project();
                        continue;
                    }
                    Poll::Pending => Poll::Pending,
                    Poll::Ready(None) => Poll::Ready(Ok(())),
                },
                Poll::Pending => Poll::Pending,
            };
        }
    }
}
