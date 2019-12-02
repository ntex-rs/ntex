use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use actix_service::{IntoService, Service};
use futures::Stream;

use crate::mpsc;

#[pin_project::pin_project]
pub struct StreamDispatcher<S, T>
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

impl<S, T> StreamDispatcher<S, T>
where
    S: Stream,
    T: Service<Request = S::Item, Response = ()> + 'static,
{
    pub fn new<F>(stream: S, service: F) -> Self
    where
        F: IntoService<T>,
    {
        let (err_tx, err_rx) = mpsc::channel();
        StreamDispatcher {
            err_rx,
            err_tx,
            stream,
            service: service.into_service(),
        }
    }
}

impl<S, T> Future for StreamDispatcher<S, T>
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
            match this.service.poll_ready(cx)? {
                Poll::Ready(_) => match this.stream.poll_next(cx) {
                    Poll::Ready(Some(item)) => {
                        actix_rt::spawn(StreamDispatcherService {
                            fut: this.service.call(item),
                            stop: self.err_tx.clone(),
                        });
                        this = self.as_mut().project();
                    }
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(None) => return Poll::Ready(Ok(())),
                },
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[pin_project::pin_project]
struct StreamDispatcherService<F: Future, E> {
    #[pin]
    fut: F,
    stop: mpsc::Sender<E>,
}

impl<F, E> Future for StreamDispatcherService<F, E>
where
    F: Future<Output = Result<(), E>>,
{
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        match this.fut.poll(cx) {
            Poll::Ready(Ok(_)) => Poll::Ready(()),
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => {
                let _ = this.stop.send(e);
                Poll::Ready(())
            }
        }
    }
}
