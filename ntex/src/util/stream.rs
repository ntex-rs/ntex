use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures::{FutureExt, Stream};

use crate::channel::mpsc;
use crate::service::{IntoService, Service};

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
        Dispatcher {
            stream,
            err_rx: mpsc::channel().1,
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
                        let stop = this.err_rx.sender();
                        crate::rt::spawn(this.service.call(item).map(move |res| {
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

#[cfg(test)]
mod tests {
    use futures::future::ok;
    use std::cell::Cell;
    use std::rc::Rc;
    use std::time::Duration;

    use super::*;
    use crate::channel::mpsc;
    use crate::rt::time::delay_for;

    #[ntex_rt::test]
    async fn test_basic() {
        let (tx, rx) = mpsc::channel();
        let counter = Rc::new(Cell::new(0));
        let counter2 = counter.clone();

        let disp = Dispatcher::new(
            rx,
            crate::fn_service(move |_: ()| {
                counter2.set(counter2.get() + 1);
                ok::<_, ()>(())
            }),
        );
        crate::rt::spawn(disp.map(|_| ()));

        tx.send(()).unwrap();
        tx.send(()).unwrap();
        drop(tx);
        delay_for(Duration::from_millis(10)).await;
        assert_eq!(counter.get(), 2);
    }
}
