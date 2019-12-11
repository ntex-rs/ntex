//! Framed dispatcher service and related utilities
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use actix_codec::{AsyncRead, AsyncWrite, Decoder, Encoder, Framed};
use actix_service::{IntoService, Service};
use actix_utils::{mpsc, oneshot};
use futures::{FutureExt, Stream};
use log::debug;

use crate::error::ServiceError;
use crate::item::Item;
use crate::sink::Sink;

type Request<S, U> = Item<S, U>;
type Response<U> = <U as Encoder>::Item;

pub(crate) enum Message<T> {
    Item(T),
    WaitClose(oneshot::Sender<()>),
    Close,
}

/// FramedTransport - is a future that reads frames from Framed object
/// and pass then to the service.
#[pin_project::pin_project]
pub(crate) struct Dispatcher<St, S, T, U>
where
    St: Clone,
    S: Service<Request = Request<St, U>, Response = Option<Response<U>>>,
    S::Error: 'static,
    S::Future: 'static,
    T: AsyncRead + AsyncWrite,
    U: Encoder + Decoder,
    <U as Encoder>::Item: 'static,
    <U as Encoder>::Error: std::fmt::Debug,
{
    service: S,
    sink: Sink<<U as Encoder>::Item>,
    state: St,
    dispatch_state: FramedState<S, U>,
    framed: Framed<T, U>,
    rx: mpsc::Receiver<Result<Message<<U as Encoder>::Item>, S::Error>>,
    tx: mpsc::Sender<Result<Message<<U as Encoder>::Item>, S::Error>>,
    disconnect: Option<Rc<dyn Fn(St, bool)>>,
}

impl<St, S, T, U> Dispatcher<St, S, T, U>
where
    St: Clone,
    S: Service<Request = Request<St, U>, Response = Option<Response<U>>>,
    S::Error: 'static,
    S::Future: 'static,
    T: AsyncRead + AsyncWrite,
    U: Decoder + Encoder,
    <U as Encoder>::Item: 'static,
    <U as Encoder>::Error: std::fmt::Debug,
{
    pub(crate) fn new<F: IntoService<S>>(
        framed: Framed<T, U>,
        state: St,
        service: F,
        sink: Sink<<U as Encoder>::Item>,
        rx: mpsc::Receiver<Result<Message<<U as Encoder>::Item>, S::Error>>,
        disconnect: Option<Rc<dyn Fn(St, bool)>>,
    ) -> Self {
        let tx = rx.sender();

        Dispatcher {
            framed,
            state,
            sink,
            disconnect,
            rx,
            tx,
            service: service.into_service(),
            dispatch_state: FramedState::Processing,
        }
    }
}

enum FramedState<S: Service, U: Encoder + Decoder> {
    Processing,
    Error(ServiceError<S::Error, U>),
    FramedError(ServiceError<S::Error, U>),
    FlushAndStop(Vec<oneshot::Sender<()>>),
    Stopping,
}

impl<S: Service, U: Encoder + Decoder> FramedState<S, U> {
    fn stop(&mut self, tx: Option<oneshot::Sender<()>>) {
        match self {
            FramedState::FlushAndStop(ref mut vec) => {
                if let Some(tx) = tx {
                    vec.push(tx)
                }
            }
            FramedState::Processing => {
                *self = FramedState::FlushAndStop(if let Some(tx) = tx {
                    vec![tx]
                } else {
                    Vec::new()
                })
            }
            FramedState::Error(_) | FramedState::FramedError(_) | FramedState::Stopping => {
                if let Some(tx) = tx {
                    let _ = tx.send(());
                }
            }
        }
    }

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

impl<St, S, T, U> Dispatcher<St, S, T, U>
where
    St: Clone,
    S: Service<Request = Request<St, U>, Response = Option<Response<U>>>,
    S::Error: 'static,
    S::Future: 'static,
    T: AsyncRead + AsyncWrite,
    U: Decoder + Encoder,
    <U as Encoder>::Item: 'static,
    <U as Encoder>::Error: std::fmt::Debug,
{
    fn poll_read(&mut self, cx: &mut Context<'_>) -> bool {
        loop {
            match self.service.poll_ready(cx) {
                Poll::Ready(Ok(_)) => {
                    let item = match self.framed.next_item(cx) {
                        Poll::Ready(Some(Ok(el))) => el,
                        Poll::Ready(Some(Err(err))) => {
                            self.dispatch_state =
                                FramedState::FramedError(ServiceError::Decoder(err));
                            return true;
                        }
                        Poll::Pending => return false,
                        Poll::Ready(None) => {
                            log::trace!("Client disconnected");
                            self.dispatch_state = FramedState::Stopping;
                            return true;
                        }
                    };

                    let tx = self.tx.clone();
                    actix_rt::spawn(
                        self.service
                            .call(Item::new(self.state.clone(), self.sink.clone(), item))
                            .map(move |item| {
                                let item = match item {
                                    Ok(Some(item)) => Ok(Message::Item(item)),
                                    Ok(None) => return,
                                    Err(err) => Err(err),
                                };
                                let _ = tx.send(item);
                            }),
                    );
                }
                Poll::Pending => return false,
                Poll::Ready(Err(err)) => {
                    self.dispatch_state = FramedState::Error(ServiceError::Service(err));
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
                    Poll::Ready(Some(Ok(Message::Item(msg)))) => {
                        if let Err(err) = self.framed.write(msg) {
                            self.dispatch_state =
                                FramedState::FramedError(ServiceError::Encoder(err));
                            return true;
                        }
                    }
                    Poll::Ready(Some(Ok(Message::Close))) => {
                        self.dispatch_state.stop(None);
                        return true;
                    }
                    Poll::Ready(Some(Ok(Message::WaitClose(tx)))) => {
                        self.dispatch_state.stop(Some(tx));
                        return true;
                    }
                    Poll::Ready(Some(Err(err))) => {
                        self.dispatch_state = FramedState::Error(ServiceError::Service(err));
                        return true;
                    }
                    Poll::Ready(None) | Poll::Pending => break,
                }
            }

            if !self.framed.is_write_buf_empty() {
                match self.framed.flush(cx) {
                    Poll::Pending => break,
                    Poll::Ready(Ok(_)) => (),
                    Poll::Ready(Err(err)) => {
                        debug!("Error sending data: {:?}", err);
                        self.dispatch_state =
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
        match self.dispatch_state {
            FramedState::Processing => {
                if self.poll_read(cx) || self.poll_write(cx) {
                    self.poll(cx)
                } else {
                    Poll::Pending
                }
            }
            FramedState::Error(_) => {
                // flush write buffer
                if !self.framed.is_write_buf_empty() {
                    if let Poll::Pending = self.framed.flush(cx) {
                        return Poll::Pending;
                    }
                }
                if let Some(ref disconnect) = self.disconnect {
                    (&*disconnect)(self.state.clone(), true);
                }
                Poll::Ready(Err(self.dispatch_state.take_error()))
            }
            FramedState::FlushAndStop(ref mut vec) => {
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
                for tx in vec.drain(..) {
                    let _ = tx.send(());
                }
                if let Some(ref disconnect) = self.disconnect {
                    (&*disconnect)(self.state.clone(), false);
                }
                Poll::Ready(Ok(()))
            }
            FramedState::FramedError(_) => {
                if let Some(ref disconnect) = self.disconnect {
                    (&*disconnect)(self.state.clone(), true);
                }
                Poll::Ready(Err(self.dispatch_state.take_framed_error()))
            }
            FramedState::Stopping => {
                if let Some(ref disconnect) = self.disconnect {
                    (&*disconnect)(self.state.clone(), false);
                }
                Poll::Ready(Ok(()))
            }
        }
    }
}
