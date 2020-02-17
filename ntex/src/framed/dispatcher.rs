//! Framed dispatcher service and related utilities
use std::pin::Pin;
use std::task::{Context, Poll};

use actix_codec::{AsyncRead, AsyncWrite, Decoder, Encoder, Framed};
use actix_service::Service;
use actix_utils::mpsc;
use futures::Stream;
use log::debug;

use super::error::ServiceError;

type Request<U> = <U as Decoder>::Item;
type Response<U> = <U as Encoder>::Item;

/// FramedTransport - is a future that reads frames from Framed object
/// and pass then to the service.
pub(crate) struct Dispatcher<S, T, U, Out>
where
    S: Service<Request = Request<U>, Response = Option<Response<U>>>,
    S::Error: 'static,
    S::Future: 'static,
    T: AsyncRead + AsyncWrite,
    U: Encoder + Decoder,
    <U as Encoder>::Item: 'static,
    <U as Encoder>::Error: std::fmt::Debug,
    Out: Stream<Item = <U as Encoder>::Item> + Unpin,
{
    service: S,
    sink: Option<Out>,
    state: FramedState<S, U>,
    framed: Framed<T, U>,
    rx: mpsc::Receiver<Result<<U as Encoder>::Item, S::Error>>,
}

impl<S, T, U, Out> Dispatcher<S, T, U, Out>
where
    S: Service<Request = Request<U>, Response = Option<Response<U>>>,
    S::Error: 'static,
    S::Future: 'static,
    T: AsyncRead + AsyncWrite,
    U: Decoder + Encoder,
    <U as Encoder>::Item: 'static,
    <U as Encoder>::Error: std::fmt::Debug,
    Out: Stream<Item = <U as Encoder>::Item> + Unpin,
{
    pub(crate) fn new(framed: Framed<T, U>, service: S, sink: Option<Out>) -> Self {
        Dispatcher {
            sink,
            service,
            framed,
            rx: mpsc::channel().1,
            state: FramedState::Processing,
        }
    }
}

enum FramedState<S: Service, U: Encoder + Decoder> {
    Processing,
    Error(ServiceError<S::Error, U>),
    FramedError(ServiceError<S::Error, U>),
    FlushAndStop,
    Stopping,
}

impl<S: Service, U: Encoder + Decoder> FramedState<S, U> {
    fn take_error(&mut self) -> ServiceError<S::Error, U> {
        match std::mem::replace(self, FramedState::Processing) {
            FramedState::Error(err) => err,
            _ => panic!(),
        }
    }

    fn take_framed_error(&mut self) -> ServiceError<S::Error, U> {
        match std::mem::replace(self, FramedState::Processing) {
            FramedState::FramedError(err) => err,
            _ => panic!(),
        }
    }
}

impl<S, T, U, Out> Dispatcher<S, T, U, Out>
where
    S: Service<Request = Request<U>, Response = Option<Response<U>>>,
    S::Error: 'static,
    S::Future: 'static,
    T: AsyncRead + AsyncWrite,
    U: Decoder + Encoder,
    <U as Encoder>::Item: 'static,
    <U as Encoder>::Error: std::fmt::Debug,
    Out: Stream<Item = <U as Encoder>::Item> + Unpin,
{
    fn poll_read(&mut self, cx: &mut Context<'_>) -> bool {
        loop {
            match self.service.poll_ready(cx) {
                Poll::Ready(Ok(_)) => {
                    let item = match self.framed.next_item(cx) {
                        Poll::Ready(Some(Ok(el))) => el,
                        Poll::Ready(Some(Err(err))) => {
                            self.state =
                                FramedState::FramedError(ServiceError::Decoder(err));
                            return true;
                        }
                        Poll::Pending => return false,
                        Poll::Ready(None) => {
                            log::trace!("Client disconnected");
                            self.state = FramedState::Stopping;
                            return true;
                        }
                    };

                    let tx = self.rx.sender();
                    let fut = self.service.call(item);
                    actix_rt::spawn(async move {
                        let item = fut.await;
                        let item = match item {
                            Ok(Some(item)) => Ok(item),
                            Ok(None) => return,
                            Err(err) => Err(err),
                        };
                        let _ = tx.send(item);
                    });
                }
                Poll::Pending => return false,
                Poll::Ready(Err(err)) => {
                    self.state = FramedState::Error(ServiceError::Service(err));
                    return true;
                }
            }
        }
    }

    /// write to framed object
    fn poll_write(&mut self, cx: &mut Context<'_>) -> bool {
        loop {
            while !self.framed.is_write_buf_full() {
                match Pin::new(&mut self.rx).poll_next(cx) {
                    Poll::Ready(Some(Ok(msg))) => {
                        if let Err(err) = self.framed.write(msg) {
                            self.state =
                                FramedState::FramedError(ServiceError::Encoder(err));
                            return true;
                        }
                        continue;
                    }
                    Poll::Ready(Some(Err(err))) => {
                        self.state = FramedState::Error(ServiceError::Service(err));
                        return true;
                    }
                    Poll::Ready(None) | Poll::Pending => (),
                }

                if self.sink.is_some() {
                    match Pin::new(self.sink.as_mut().unwrap()).poll_next(cx) {
                        Poll::Ready(Some(msg)) => {
                            if let Err(err) = self.framed.write(msg) {
                                self.state =
                                    FramedState::FramedError(ServiceError::Encoder(err));
                                return true;
                            }
                            continue;
                        }
                        Poll::Ready(None) => {
                            let _ = self.sink.take();
                            self.state = FramedState::FlushAndStop;
                            return true;
                        }
                        Poll::Pending => (),
                    }
                }
                break;
            }

            if !self.framed.is_write_buf_empty() {
                match self.framed.flush(cx) {
                    Poll::Pending => break,
                    Poll::Ready(Ok(_)) => (),
                    Poll::Ready(Err(err)) => {
                        debug!("Error sending data: {:?}", err);
                        self.state =
                            FramedState::FramedError(ServiceError::Encoder(err));
                        return true;
                    }
                }
            } else {
                break;
            }
        }
        false
    }

    pub(crate) fn poll(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), ServiceError<S::Error, U>>> {
        match self.state {
            FramedState::Processing => loop {
                let read = self.poll_read(cx);
                let write = self.poll_write(cx);
                if read || write {
                    continue;
                } else {
                    return Poll::Pending;
                }
            },
            FramedState::Error(_) => {
                // flush write buffer
                if !self.framed.is_write_buf_empty() {
                    if let Poll::Pending = self.framed.flush(cx) {
                        return Poll::Pending;
                    }
                }
                Poll::Ready(Err(self.state.take_error()))
            }
            FramedState::FlushAndStop => {
                // drain service responses
                match Pin::new(&mut self.rx).poll_next(cx) {
                    Poll::Ready(Some(Ok(msg))) => {
                        if let Err(_) = self.framed.write(msg) {
                            return Poll::Ready(Ok(()));
                        }
                    }
                    Poll::Ready(Some(Err(_))) => return Poll::Ready(Ok(())),
                    Poll::Ready(None) | Poll::Pending => (),
                }

                // flush io
                if !self.framed.is_write_buf_empty() {
                    match self.framed.flush(cx) {
                        Poll::Ready(Err(err)) => {
                            debug!("Error sending data: {:?}", err);
                        }
                        Poll::Pending => {
                            return Poll::Pending;
                        }
                        Poll::Ready(_) => (),
                    }
                };
                Poll::Ready(Ok(()))
            }
            FramedState::FramedError(_) => {
                Poll::Ready(Err(self.state.take_framed_error()))
            }
            FramedState::Stopping => Poll::Ready(Ok(())),
        }
    }
}
