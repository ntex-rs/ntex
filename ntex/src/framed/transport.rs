//! Framed dispatcher service and related utilities
#![allow(type_alias_bounds)]
use std::mem;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures::{Future, FutureExt, Stream};
use log::debug;

use crate::channel::mpsc;
use crate::codec::{AsyncRead, AsyncWrite, Decoder, Encoder, Framed};
use crate::service::{IntoService, Service};

use super::error::ServiceError;

type Request<U> = <U as Decoder>::Item;
type Response<U> = <U as Encoder>::Item;

pub enum Message<T> {
    Item(T),
    Close,
}

/// FramedTransport - is a future that reads frames from Framed object
/// and pass then to the service.
#[pin_project::pin_project]
pub struct Dispatcher<S, T, U>
where
    S: Service<Request = Request<U>, Response = Response<U>>,
    S::Error: 'static,
    S::Future: 'static,
    T: AsyncRead + AsyncWrite,
    U: Encoder + Decoder,
    <U as Encoder>::Item: 'static,
    <U as Encoder>::Error: std::fmt::Debug,
{
    service: S,
    state: State<S, U>,
    framed: Framed<T, U>,
    rx: mpsc::Receiver<Result<Message<<U as Encoder>::Item>, S::Error>>,
    tx: mpsc::Sender<Result<Message<<U as Encoder>::Item>, S::Error>>,
}

enum State<S: Service, U: Encoder + Decoder> {
    Processing,
    Error(ServiceError<S::Error, U>),
    FramedError(ServiceError<S::Error, U>),
    FlushAndStop,
    Stopping,
}

impl<S: Service, U: Encoder + Decoder> State<S, U> {
    fn take_error(&mut self) -> ServiceError<S::Error, U> {
        match mem::replace(self, State::Processing) {
            State::Error(err) => err,
            _ => panic!(),
        }
    }

    fn take_framed_error(&mut self) -> ServiceError<S::Error, U> {
        match mem::replace(self, State::Processing) {
            State::FramedError(err) => err,
            _ => panic!(),
        }
    }
}

impl<S, T, U> Dispatcher<S, T, U>
where
    S: Service<Request = Request<U>, Response = Response<U>>,
    S::Error: 'static,
    S::Future: 'static,
    T: AsyncRead + AsyncWrite,
    U: Decoder + Encoder,
    <U as Encoder>::Item: 'static,
    <U as Encoder>::Error: std::fmt::Debug,
{
    pub fn new<F: IntoService<S>>(framed: Framed<T, U>, service: F) -> Self {
        let (tx, rx) = mpsc::channel();
        Dispatcher {
            framed,
            rx,
            tx,
            service: service.into_service(),
            state: State::Processing,
        }
    }

    /// Construct new `Dispatcher` instance with customer `mpsc::Receiver`
    pub fn with_rx<F: IntoService<S>>(
        framed: Framed<T, U>,
        service: F,
        rx: mpsc::Receiver<Result<Message<<U as Encoder>::Item>, S::Error>>,
    ) -> Self {
        let tx = rx.sender();
        Dispatcher {
            framed,
            rx,
            tx,
            service: service.into_service(),
            state: State::Processing,
        }
    }

    /// Get sink
    pub fn get_sink(
        &self,
    ) -> mpsc::Sender<Result<Message<<U as Encoder>::Item>, S::Error>> {
        self.tx.clone()
    }

    /// Get reference to a service wrapped by `Dispatcher` instance.
    pub fn get_ref(&self) -> &S {
        &self.service
    }

    /// Get mutable reference to a service wrapped by `Dispatcher` instance.
    pub fn get_mut(&mut self) -> &mut S {
        &mut self.service
    }

    /// Get reference to a framed instance wrapped by `Dispatcher`
    /// instance.
    pub fn get_framed(&self) -> &Framed<T, U> {
        &self.framed
    }

    /// Get mutable reference to a framed instance wrapped by `Dispatcher` instance.
    pub fn get_framed_mut(&mut self) -> &mut Framed<T, U> {
        &mut self.framed
    }

    fn poll_read(&mut self, cx: &mut Context<'_>) -> bool
    where
        S: Service<Request = Request<U>, Response = Response<U>>,
        S::Error: 'static,
        S::Future: 'static,
        T: AsyncRead + AsyncWrite,
        U: Decoder + Encoder,
        <U as Encoder>::Item: 'static,
        <U as Encoder>::Error: std::fmt::Debug,
    {
        loop {
            match self.service.poll_ready(cx) {
                Poll::Ready(Ok(_)) => {
                    let item = match self.framed.next_item(cx) {
                        Poll::Ready(Some(Ok(el))) => el,
                        Poll::Ready(Some(Err(err))) => {
                            self.state = State::FramedError(ServiceError::Decoder(err));
                            return true;
                        }
                        Poll::Pending => return false,
                        Poll::Ready(None) => {
                            self.state = State::Stopping;
                            return true;
                        }
                    };

                    let tx = self.tx.clone();
                    crate::rt::spawn(self.service.call(item).map(move |item| {
                        let _ = tx.send(item.map(Message::Item));
                    }));
                }
                Poll::Pending => return false,
                Poll::Ready(Err(err)) => {
                    self.state = State::Error(ServiceError::Service(err));
                    return true;
                }
            }
        }
    }

    /// write to framed object
    fn poll_write(&mut self, cx: &mut Context<'_>) -> bool
    where
        S: Service<Request = Request<U>, Response = Response<U>>,
        S::Error: 'static,
        S::Future: 'static,
        T: AsyncRead + AsyncWrite,
        U: Decoder + Encoder,
        <U as Encoder>::Item: 'static,
        <U as Encoder>::Error: std::fmt::Debug,
    {
        loop {
            while !self.framed.is_write_buf_full() {
                match Pin::new(&mut self.rx).poll_next(cx) {
                    Poll::Ready(Some(Ok(Message::Item(msg)))) => {
                        if let Err(err) = self.framed.write(msg) {
                            self.state = State::FramedError(ServiceError::Encoder(err));
                            return true;
                        }
                    }
                    Poll::Ready(Some(Ok(Message::Close))) => {
                        self.state = State::FlushAndStop;
                        return true;
                    }
                    Poll::Ready(Some(Err(err))) => {
                        self.state = State::Error(ServiceError::Service(err));
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
                        self.state = State::FramedError(ServiceError::Encoder(err));
                        return true;
                    }
                }
            } else {
                break;
            }
        }

        false
    }
}

impl<S, T, U> Future for Dispatcher<S, T, U>
where
    S: Service<Request = Request<U>, Response = Response<U>>,
    S::Error: 'static,
    S::Future: 'static,
    T: AsyncRead + AsyncWrite,
    U: Decoder + Encoder,
    <U as Encoder>::Item: 'static,
    <U as Encoder>::Error: std::fmt::Debug,
    <U as Decoder>::Error: std::fmt::Debug,
{
    type Output = Result<(), ServiceError<S::Error, U>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        loop {
            let this = self.as_mut().project();

            return match this.state {
                State::Processing => {
                    if self.poll_read(cx) || self.poll_write(cx) {
                        continue;
                    } else {
                        Poll::Pending
                    }
                }
                State::Error(_) => {
                    // flush write buffer
                    if !self.framed.is_write_buf_empty() {
                        if let Poll::Pending = self.framed.flush(cx) {
                            return Poll::Pending;
                        }
                    }
                    Poll::Ready(Err(self.state.take_error()))
                }
                State::FlushAndStop => {
                    if !this.framed.is_write_buf_empty() {
                        match this.framed.flush(cx) {
                            Poll::Ready(Err(err)) => {
                                debug!("Error sending data: {:?}", err);
                                Poll::Ready(Ok(()))
                            }
                            Poll::Pending => Poll::Pending,
                            Poll::Ready(Ok(_)) => Poll::Ready(Ok(())),
                        }
                    } else {
                        Poll::Ready(Ok(()))
                    }
                }
                State::FramedError(_) => {
                    Poll::Ready(Err(this.state.take_framed_error()))
                }
                State::Stopping => Poll::Ready(Ok(())),
            };
        }
    }
}
