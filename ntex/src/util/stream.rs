use std::{fmt, future::Future, pin::Pin, task::Context, task::Poll};

use crate::channel::mpsc;
use crate::service::{IntoService, Service};
use crate::{util::poll_fn, Sink, Stream};

pin_project_lite::pin_project! {
    pub struct Dispatcher<Req, R, S, T, U>
    where
        R: 'static,
        S: Service<Req, Response = Option<R>>,
        S: 'static,
        T: Stream<Item = Result<Req, S::Error>>,
        T: Unpin,
        U: Sink<Result<R, S::Error>>,
        U: Unpin,
    {
        #[pin]
        service: S,
        stream: T,
        sink: Option<U>,
        rx: mpsc::Receiver<Result<S::Response, S::Error>>,
        shutdown: Option<bool>,
    }
}

impl<Req, R, S, T, U> Dispatcher<Req, R, S, T, U>
where
    R: 'static,
    S: Service<Req, Response = Option<R>> + 'static,
    S::Error: fmt::Debug,
    T: Stream<Item = Result<Req, S::Error>> + Unpin,
    U: Sink<Result<R, S::Error>> + Unpin + 'static,
    U::Error: fmt::Debug,
{
    pub fn new<F>(stream: T, sink: U, service: F) -> Self
    where
        F: IntoService<S, Req>,
    {
        Dispatcher {
            stream,
            sink: Some(sink),
            service: service.into_service(),
            rx: mpsc::channel().1,
            shutdown: None,
        }
    }
}

impl<Req, R, S, T, U> Future for Dispatcher<Req, R, S, T, U>
where
    R: 'static,
    S: Service<Req, Response = Option<R>> + 'static,
    S::Future: 'static,
    S::Error: fmt::Debug + 'static,
    T: Stream<Item = Result<Req, S::Error>> + Unpin,
    U: Sink<Result<R, S::Error>> + Unpin + 'static,
    U::Error: fmt::Debug,
{
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.as_mut().project();

        if let Some(is_err) = this.shutdown {
            if let Some(mut sink) = this.sink.take() {
                crate::rt::spawn(async move {
                    if poll_fn(|cx| Pin::new(&mut sink).poll_flush(cx))
                        .await
                        .is_ok()
                    {
                        let _ = poll_fn(|cx| Pin::new(&mut sink).poll_close(cx)).await;
                    }
                });
            }
            if this.service.poll_shutdown(cx, *is_err).is_pending() {
                return Poll::Pending;
            }
            return Poll::Ready(());
        }

        loop {
            match Pin::new(this.sink.as_mut().unwrap()).poll_ready(cx) {
                Poll::Pending => {
                    match Pin::new(this.sink.as_mut().unwrap()).poll_flush(cx) {
                        Poll::Pending => break,
                        Poll::Ready(Ok(_)) => (),
                        Poll::Ready(Err(e)) => {
                            trace!("Sink flush failed: {:?}", e);
                            *this.shutdown = Some(true);
                            return self.poll(cx);
                        }
                    }
                }
                Poll::Ready(Ok(_)) => {
                    if let Poll::Ready(Some(item)) = Pin::new(&mut this.rx).poll_next(cx) {
                        match item {
                            Ok(Some(item)) => {
                                if let Err(e) = Pin::new(this.sink.as_mut().unwrap())
                                    .start_send(Ok(item))
                                {
                                    trace!("Failed to write to sink: {:?}", e);
                                    *this.shutdown = Some(true);
                                    return self.poll(cx);
                                }
                                continue;
                            }
                            Ok(None) => continue,
                            Err(e) => {
                                trace!("Stream is failed: {:?}", e);
                                let _ = Pin::new(this.sink.as_mut().unwrap())
                                    .start_send(Err(e));
                                *this.shutdown = Some(true);
                                return self.poll(cx);
                            }
                        }
                    }
                }
                Poll::Ready(Err(e)) => {
                    trace!("Sink readiness check failed: {:?}", e);
                    *this.shutdown = Some(true);
                    return self.poll(cx);
                }
            }
            break;
        }

        loop {
            return match this.service.poll_ready(cx) {
                Poll::Ready(Ok(_)) => match Pin::new(&mut this.stream).poll_next(cx) {
                    Poll::Ready(Some(Ok(item))) => {
                        let tx = this.rx.sender();
                        let fut = this.service.call(item);
                        crate::rt::spawn(async move {
                            let res = fut.await;
                            let _ = tx.send(res);
                        });
                        this = self.as_mut().project();
                        continue;
                    }
                    Poll::Pending => Poll::Pending,
                    Poll::Ready(Some(Err(_))) => {
                        *this.shutdown = Some(true);
                        return self.poll(cx);
                    }
                    Poll::Ready(None) => {
                        *this.shutdown = Some(false);
                        return self.poll(cx);
                    }
                },
                Poll::Ready(Err(e)) => {
                    trace!("Service readiness check failed: {:?}", e);
                    *this.shutdown = Some(true);
                    return self.poll(cx);
                }
                Poll::Pending => Poll::Pending,
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use super::*;
    use crate::util::{next, ByteString, BytesMut};
    use crate::{channel::mpsc, codec::Encoder, time::sleep, time::Millis, ws};

    #[crate::rt_test]
    async fn test_basic() {
        let counter = Rc::new(Cell::new(0));
        let counter2 = counter.clone();

        let (tx1, mut rx) = mpsc::channel();
        let (tx, rx2) = mpsc::channel();
        let encoder = ws::StreamEncoder::new(tx1);
        let decoder = ws::StreamDecoder::new(rx2);

        let disp = Dispatcher::new(
            decoder,
            encoder,
            crate::service::fn_service(move |_| {
                counter2.set(counter2.get() + 1);
                async { Ok(Some(ws::Message::Text(ByteString::from_static("test")))) }
            }),
        );
        crate::rt::spawn(async move {
            let _ = disp.await;
        });

        let mut buf = BytesMut::new();
        let codec = ws::Codec::new().client_mode();
        codec
            .encode(ws::Message::Text(ByteString::from_static("test")), &mut buf)
            .unwrap();
        tx.send(Ok::<_, ()>(buf.split().freeze())).unwrap();

        let data = next(&mut rx).await.unwrap().unwrap();
        assert_eq!(data, b"\x81\x04test".as_ref());

        drop(tx);
        sleep(Millis(10)).await;
        assert!(next(&mut rx).await.is_none());

        assert_eq!(counter.get(), 1);
    }
}
