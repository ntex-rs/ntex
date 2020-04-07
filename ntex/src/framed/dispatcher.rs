//! Framed dispatcher service and related utilities
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use futures::{ready, Stream};
use log::debug;

use crate::channel::mpsc;
use crate::codec::{AsyncRead, AsyncWrite, Decoder, Encoder, Framed};
use crate::rt::time::{delay_for, Delay};
use crate::service::Service;

use super::error::ServiceError;

type Request<U> = <U as Decoder>::Item;
type Response<U> = <U as Encoder>::Item;

/// FramedTransport - is a future that reads frames from Framed object
/// and pass then to the service.
pub(super) struct Dispatcher<S, T, U, Out>
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
    disconnect_timeout: usize,
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
    pub(super) fn new(
        framed: Framed<T, U>,
        service: S,
        sink: Option<Out>,
        timeout: usize,
    ) -> Self {
        Dispatcher {
            sink,
            service,
            framed,
            rx: mpsc::channel().1,
            state: FramedState::Processing,
            disconnect_timeout: timeout,
        }
    }
}

enum FramedState<S: Service, U: Encoder + Decoder> {
    Processing,
    Error(ServiceError<S::Error, U>),
    FlushAndStop,
    Shutdown(Option<ServiceError<S::Error, U>>),
    ShutdownIo(Delay),
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum PollResult {
    Continue,
    Pending,
}

impl<S: Service, U: Encoder + Decoder> FramedState<S, U> {
    fn take_error(&mut self) -> ServiceError<S::Error, U> {
        match std::mem::replace(self, FramedState::Processing) {
            FramedState::Error(err) => err,
            _ => panic!(),
        }
    }
}

impl<S, T, U, Out> Dispatcher<S, T, U, Out>
where
    S: Service<Request = Request<U>, Response = Option<Response<U>>>,
    S::Error: 'static,
    S::Future: 'static,
    T: AsyncRead + AsyncWrite + Unpin,
    U: Decoder + Encoder,
    <U as Encoder>::Item: 'static,
    <U as Encoder>::Error: std::fmt::Debug,
    Out: Stream<Item = <U as Encoder>::Item> + Unpin,
{
    fn poll_read(&mut self, cx: &mut Context<'_>) -> PollResult {
        loop {
            match self.service.poll_ready(cx) {
                Poll::Ready(Ok(_)) => {
                    let item = match self.framed.next_item(cx) {
                        Poll::Ready(Some(Ok(el))) => el,
                        Poll::Ready(Some(Err(err))) => {
                            self.state =
                                FramedState::Shutdown(Some(ServiceError::Decoder(err)));
                            return PollResult::Continue;
                        }
                        Poll::Pending => return PollResult::Pending,
                        Poll::Ready(None) => {
                            log::trace!("Client disconnected");
                            self.state = FramedState::Shutdown(None);
                            return PollResult::Continue;
                        }
                    };

                    let tx = self.rx.sender();
                    let fut = self.service.call(item);
                    crate::rt::spawn(async move {
                        let item = fut.await;
                        let item = match item {
                            Ok(Some(item)) => Ok(item),
                            Ok(None) => return,
                            Err(err) => Err(err),
                        };
                        let _ = tx.send(item);
                    });
                }
                Poll::Pending => return PollResult::Pending,
                Poll::Ready(Err(err)) => {
                    self.state = FramedState::Error(ServiceError::Service(err));
                    return PollResult::Continue;
                }
            }
        }
    }

    /// write to framed object
    fn poll_write(&mut self, cx: &mut Context<'_>) -> PollResult {
        loop {
            while !self.framed.is_write_buf_full() {
                match Pin::new(&mut self.rx).poll_next(cx) {
                    Poll::Ready(Some(Ok(msg))) => {
                        if let Err(err) = self.framed.write(msg) {
                            self.state =
                                FramedState::Shutdown(Some(ServiceError::Encoder(err)));
                            return PollResult::Continue;
                        }
                        continue;
                    }
                    Poll::Ready(Some(Err(err))) => {
                        self.state = FramedState::Error(ServiceError::Service(err));
                        return PollResult::Continue;
                    }
                    Poll::Ready(None) | Poll::Pending => {}
                }

                if self.sink.is_some() {
                    match Pin::new(self.sink.as_mut().unwrap()).poll_next(cx) {
                        Poll::Ready(Some(msg)) => {
                            if let Err(err) = self.framed.write(msg) {
                                self.state = FramedState::Shutdown(Some(
                                    ServiceError::Encoder(err),
                                ));
                                return PollResult::Continue;
                            }
                            continue;
                        }
                        Poll::Ready(None) => {
                            let _ = self.sink.take();
                            self.state = FramedState::FlushAndStop;
                            return PollResult::Continue;
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
                            FramedState::Shutdown(Some(ServiceError::Encoder(err)));
                        return PollResult::Continue;
                    }
                }
            } else {
                break;
            }
        }
        PollResult::Pending
    }

    pub(super) fn poll(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), ServiceError<S::Error, U>>> {
        loop {
            match self.state {
                FramedState::Processing => {
                    let read = self.poll_read(cx);
                    let write = self.poll_write(cx);
                    if read == PollResult::Continue || write == PollResult::Continue {
                        continue;
                    } else {
                        return Poll::Pending;
                    }
                }
                FramedState::Error(_) => {
                    // flush write buffer
                    if !self.framed.is_write_buf_empty() {
                        if let Poll::Pending = self.framed.flush(cx) {
                            return Poll::Pending;
                        }
                    }
                    self.state = FramedState::Shutdown(Some(self.state.take_error()));
                }
                FramedState::FlushAndStop => {
                    // drain service responses
                    match Pin::new(&mut self.rx).poll_next(cx) {
                        Poll::Ready(Some(Ok(msg))) => {
                            if let Err(err) = self.framed.write(msg) {
                                self.state = FramedState::Shutdown(Some(
                                    ServiceError::Encoder(err),
                                ));
                                continue;
                            }
                        }
                        Poll::Ready(Some(Err(err))) => {
                            self.state = FramedState::Shutdown(Some(err.into()));
                            continue;
                        }
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
                    self.state = FramedState::Shutdown(None);
                }
                FramedState::Shutdown(ref mut err) => {
                    return if self.service.poll_shutdown(cx, err.is_some()).is_ready() {
                        if let Some(err) = err.take() {
                            Poll::Ready(Err(err))
                        } else {
                            let pending = self.framed.close(cx).is_pending();
                            if self.disconnect_timeout == 0 && pending {
                                self.state = FramedState::ShutdownIo(delay_for(
                                    Duration::from_millis(
                                        self.disconnect_timeout as u64,
                                    ),
                                ));
                                continue;
                            } else {
                                Poll::Ready(Ok(()))
                            }
                        }
                    } else {
                        Poll::Pending
                    }
                }
                FramedState::ShutdownIo(ref mut delay) => {
                    if let Poll::Ready(res) = self.framed.close(cx) {
                        return Poll::Ready(
                            res.map_err(|e| ServiceError::Encoder(e.into())),
                        );
                    } else {
                        ready!(Pin::new(delay).poll(cx));
                        return Poll::Ready(Ok(()));
                    }
                }
            }
        }
    }
}
