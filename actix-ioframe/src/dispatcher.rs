//! Framed dispatcher service and related utilities
use std::collections::VecDeque;
use std::mem;
use std::rc::Rc;

use actix_codec::{AsyncRead, AsyncWrite, Decoder, Encoder, Framed};
use actix_service::{IntoService, Service};
use futures::task::AtomicTask;
use futures::unsync::{mpsc, oneshot};
use futures::{Async, Future, Poll, Sink as FutureSink, Stream};
use log::debug;

use crate::cell::Cell;
use crate::error::ServiceError;
use crate::item::Item;
use crate::sink::Sink;
use crate::state::State;

type Request<S, U> = Item<S, U>;
type Response<U> = <U as Encoder>::Item;

pub(crate) enum FramedMessage<T> {
    Message(T),
    Close,
    WaitClose(oneshot::Sender<()>),
}

/// FramedTransport - is a future that reads frames from Framed object
/// and pass then to the service.
pub(crate) struct FramedDispatcher<St, S, T, U>
where
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
    state: State<St>,
    dispatch_state: FramedState<S, U>,
    framed: Framed<T, U>,
    rx: Option<mpsc::UnboundedReceiver<FramedMessage<<U as Encoder>::Item>>>,
    inner: Cell<FramedDispatcherInner<<U as Encoder>::Item, S::Error>>,
    disconnect: Option<Rc<Fn(&mut St, bool)>>,
}

impl<St, S, T, U> FramedDispatcher<St, S, T, U>
where
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
        state: State<St>,
        service: F,
        rx: mpsc::UnboundedReceiver<FramedMessage<<U as Encoder>::Item>>,
        sink: Sink<<U as Encoder>::Item>,
        disconnect: Option<Rc<Fn(&mut St, bool)>>,
    ) -> Self {
        FramedDispatcher {
            framed,
            state,
            sink,
            disconnect,
            rx: Some(rx),
            service: service.into_service(),
            dispatch_state: FramedState::Processing,
            inner: Cell::new(FramedDispatcherInner {
                buf: VecDeque::new(),
                task: AtomicTask::new(),
            }),
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
}

struct FramedDispatcherInner<I, E> {
    buf: VecDeque<Result<I, E>>,
    task: AtomicTask,
}

impl<St, S, T, U> FramedDispatcher<St, S, T, U>
where
    S: Service<Request = Request<St, U>, Response = Option<Response<U>>>,
    S::Error: 'static,
    S::Future: 'static,
    T: AsyncRead + AsyncWrite,
    U: Decoder + Encoder,
    <U as Encoder>::Item: 'static,
    <U as Encoder>::Error: std::fmt::Debug,
{
    fn disconnect(&mut self, error: bool) {
        if let Some(ref disconnect) = self.disconnect {
            (&*disconnect)(&mut *self.state.get_mut(), error);
        }
    }

    fn poll_read(&mut self) -> bool {
        loop {
            match self.service.poll_ready() {
                Ok(Async::Ready(_)) => {
                    let item = match self.framed.poll() {
                        Ok(Async::Ready(Some(el))) => el,
                        Err(err) => {
                            self.dispatch_state =
                                FramedState::FramedError(ServiceError::Decoder(err));
                            return true;
                        }
                        Ok(Async::NotReady) => return false,
                        Ok(Async::Ready(None)) => {
                            self.dispatch_state = FramedState::Stopping;
                            return true;
                        }
                    };

                    let mut cell = self.inner.clone();
                    cell.get_mut().task.register();
                    tokio_current_thread::spawn(
                        self.service
                            .call(Item::new(self.state.clone(), self.sink.clone(), item))
                            .then(move |item| {
                                let item = match item {
                                    Ok(Some(item)) => Ok(item),
                                    Ok(None) => return Ok(()),
                                    Err(err) => Err(err),
                                };
                                let inner = cell.get_mut();
                                inner.buf.push_back(item);
                                inner.task.notify();
                                Ok(())
                            }),
                    );
                }
                Ok(Async::NotReady) => return false,
                Err(err) => {
                    self.dispatch_state = FramedState::Error(ServiceError::Service(err));
                    return true;
                }
            }
        }
    }

    /// write to framed object
    fn poll_write(&mut self) -> bool {
        let inner = self.inner.get_mut();
        let mut rx_done = self.rx.is_none();
        let mut buf_empty = inner.buf.is_empty();
        loop {
            while !self.framed.is_write_buf_full() {
                if !buf_empty {
                    match inner.buf.pop_front().unwrap() {
                        Ok(msg) => {
                            if let Err(err) = self.framed.force_send(msg) {
                                self.dispatch_state =
                                    FramedState::FramedError(ServiceError::Encoder(err));
                                return true;
                            }
                            buf_empty = inner.buf.is_empty();
                        }
                        Err(err) => {
                            self.dispatch_state =
                                FramedState::Error(ServiceError::Service(err));
                            return true;
                        }
                    }
                }

                if !rx_done && self.rx.is_some() {
                    match self.rx.as_mut().unwrap().poll() {
                        Ok(Async::Ready(Some(FramedMessage::Message(msg)))) => {
                            if let Err(err) = self.framed.force_send(msg) {
                                self.dispatch_state =
                                    FramedState::FramedError(ServiceError::Encoder(err));
                                return true;
                            }
                        }
                        Ok(Async::Ready(Some(FramedMessage::Close))) => {
                            self.dispatch_state.stop(None);
                            return true;
                        }
                        Ok(Async::Ready(Some(FramedMessage::WaitClose(tx)))) => {
                            self.dispatch_state.stop(Some(tx));
                            return true;
                        }
                        Ok(Async::Ready(None)) => {
                            rx_done = true;
                            let _ = self.rx.take();
                        }
                        Ok(Async::NotReady) => rx_done = true,
                        Err(_e) => {
                            rx_done = true;
                            let _ = self.rx.take();
                        }
                    }
                }

                if rx_done && buf_empty {
                    break;
                }
            }

            if !self.framed.is_write_buf_empty() {
                match self.framed.poll_complete() {
                    Ok(Async::NotReady) => break,
                    Err(err) => {
                        debug!("Error sending data: {:?}", err);
                        self.dispatch_state =
                            FramedState::FramedError(ServiceError::Encoder(err));
                        return true;
                    }
                    Ok(Async::Ready(_)) => (),
                }
            } else {
                break;
            }
        }

        false
    }
}

impl<St, S, T, U> Future for FramedDispatcher<St, S, T, U>
where
    S: Service<Request = Request<St, U>, Response = Option<Response<U>>>,
    S::Error: 'static,
    S::Future: 'static,
    T: AsyncRead + AsyncWrite,
    U: Decoder + Encoder,
    <U as Encoder>::Item: 'static,
    <U as Encoder>::Error: std::fmt::Debug,
{
    type Item = ();
    type Error = ServiceError<S::Error, U>;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match mem::replace(&mut self.dispatch_state, FramedState::Processing) {
            FramedState::Processing => {
                if self.poll_read() || self.poll_write() {
                    self.poll()
                } else {
                    Ok(Async::NotReady)
                }
            }
            FramedState::Error(err) => {
                if self.framed.is_write_buf_empty()
                    || (self.poll_write() || self.framed.is_write_buf_empty())
                {
                    self.disconnect(true);
                    Err(err)
                } else {
                    self.dispatch_state = FramedState::Error(err);
                    Ok(Async::NotReady)
                }
            }
            FramedState::FlushAndStop(mut vec) => {
                if !self.framed.is_write_buf_empty() {
                    match self.framed.poll_complete() {
                        Err(err) => {
                            debug!("Error sending data: {:?}", err);
                        }
                        Ok(Async::NotReady) => {
                            self.dispatch_state = FramedState::FlushAndStop(vec);
                            return Ok(Async::NotReady);
                        }
                        Ok(Async::Ready(_)) => (),
                    }
                };
                for tx in vec.drain(..) {
                    let _ = tx.send(());
                }
                self.disconnect(false);
                Ok(Async::Ready(()))
            }
            FramedState::FramedError(err) => {
                self.disconnect(true);
                Err(err)
            }
            FramedState::Stopping => {
                self.disconnect(false);
                Ok(Async::Ready(()))
            }
        }
    }
}
