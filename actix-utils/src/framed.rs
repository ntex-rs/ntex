//! Framed dispatcher service and related utilities
#![allow(type_alias_bounds)]
use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::{fmt, mem};

use actix_codec::{AsyncRead, AsyncWrite, Decoder, Encoder, Framed};
use actix_service::{IntoService, Service};
use futures::future::{ready, FutureExt};
use futures::{Future, Sink, Stream};
use log::debug;

use crate::cell::Cell;
use crate::mpsc;
use crate::task::LocalWaker;

type Request<U> = <U as Decoder>::Item;
type Response<U> = <U as Encoder>::Item;

/// Framed transport errors
pub enum FramedTransportError<E, U: Encoder + Decoder> {
    Service(E),
    Encoder(<U as Encoder>::Error),
    Decoder(<U as Decoder>::Error),
}

impl<E, U: Encoder + Decoder> From<E> for FramedTransportError<E, U> {
    fn from(err: E) -> Self {
        FramedTransportError::Service(err)
    }
}

impl<E, U: Encoder + Decoder> fmt::Debug for FramedTransportError<E, U>
where
    E: fmt::Debug,
    <U as Encoder>::Error: fmt::Debug,
    <U as Decoder>::Error: fmt::Debug,
{
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            FramedTransportError::Service(ref e) => {
                write!(fmt, "FramedTransportError::Service({:?})", e)
            }
            FramedTransportError::Encoder(ref e) => {
                write!(fmt, "FramedTransportError::Encoder({:?})", e)
            }
            FramedTransportError::Decoder(ref e) => {
                write!(fmt, "FramedTransportError::Encoder({:?})", e)
            }
        }
    }
}

impl<E, U: Encoder + Decoder> fmt::Display for FramedTransportError<E, U>
where
    E: fmt::Display,
    <U as Encoder>::Error: fmt::Debug,
    <U as Decoder>::Error: fmt::Debug,
{
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            FramedTransportError::Service(ref e) => write!(fmt, "{}", e),
            FramedTransportError::Encoder(ref e) => write!(fmt, "{:?}", e),
            FramedTransportError::Decoder(ref e) => write!(fmt, "{:?}", e),
        }
    }
}

pub enum FramedMessage<T> {
    Message(T),
    Close,
}

type Rx<U> = Option<mpsc::Receiver<FramedMessage<<U as Encoder>::Item>>>;
type Inner<S: Service, U> = Cell<FramedTransportInner<<U as Encoder>::Item, S::Error>>;

/// FramedTransport - is a future that reads frames from Framed object
/// and pass then to the service.
#[pin_project::pin_project]
pub struct FramedTransport<S, T, U>
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
    state: TransportState<S, U>,
    framed: Framed<T, U>,
    rx: Option<mpsc::Receiver<FramedMessage<<U as Encoder>::Item>>>,
    inner: Cell<FramedTransportInner<<U as Encoder>::Item, S::Error>>,
}

enum TransportState<S: Service, U: Encoder + Decoder> {
    Processing,
    Error(FramedTransportError<S::Error, U>),
    FramedError(FramedTransportError<S::Error, U>),
    FlushAndStop,
    Stopping,
}

struct FramedTransportInner<I, E> {
    buf: VecDeque<Result<I, E>>,
    task: LocalWaker,
}

impl<S, T, U> FramedTransport<S, T, U>
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
        FramedTransport {
            framed,
            rx: None,
            service: service.into_service(),
            state: TransportState::Processing,
            inner: Cell::new(FramedTransportInner {
                buf: VecDeque::new(),
                task: LocalWaker::new(),
            }),
        }
    }

    /// Get Sender
    pub fn set_receiver(
        mut self,
        rx: mpsc::Receiver<FramedMessage<<U as Encoder>::Item>>,
    ) -> Self {
        self.rx = Some(rx);
        self
    }

    /// Get reference to a service wrapped by `FramedTransport` instance.
    pub fn get_ref(&self) -> &S {
        &self.service
    }

    /// Get mutable reference to a service wrapped by `FramedTransport`
    /// instance.
    pub fn get_mut(&mut self) -> &mut S {
        &mut self.service
    }

    /// Get reference to a framed instance wrapped by `FramedTransport`
    /// instance.
    pub fn get_framed(&self) -> &Framed<T, U> {
        &self.framed
    }

    /// Get mutable reference to a framed instance wrapped by `FramedTransport`
    /// instance.
    pub fn get_framed_mut(&mut self) -> &mut Framed<T, U> {
        &mut self.framed
    }
}

impl<S, T, U> Future for FramedTransport<S, T, U>
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
    type Output = Result<(), FramedTransportError<S::Error, U>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.inner.get_ref().task.register(cx.waker());

        let this = self.project();
        poll(
            cx,
            this.service,
            this.state,
            this.framed,
            this.rx,
            this.inner,
        )
    }
}

fn poll<S, T, U>(
    cx: &mut Context<'_>,
    srv: &mut S,
    state: &mut TransportState<S, U>,
    framed: &mut Framed<T, U>,
    rx: &mut Rx<U>,
    inner: &mut Inner<S, U>,
) -> Poll<Result<(), FramedTransportError<S::Error, U>>>
where
    S: Service<Request = Request<U>, Response = Response<U>>,
    S::Error: 'static,
    S::Future: 'static,
    T: AsyncRead + AsyncWrite,
    U: Decoder + Encoder,
    <U as Encoder>::Item: 'static,
    <U as Encoder>::Error: std::fmt::Debug,
{
    match mem::replace(state, TransportState::Processing) {
        TransportState::Processing => {
            if poll_read(cx, srv, state, framed, inner)
                || poll_write(cx, state, framed, rx, inner)
            {
                poll(cx, srv, state, framed, rx, inner)
            } else {
                Poll::Pending
            }
        }
        TransportState::Error(err) => {
            let is_empty = framed.is_write_buf_empty();
            if is_empty || poll_write(cx, state, framed, rx, inner) {
                Poll::Ready(Err(err))
            } else {
                *state = TransportState::Error(err);
                Poll::Pending
            }
        }
        TransportState::FlushAndStop => {
            if !framed.is_write_buf_empty() {
                match Pin::new(framed).poll_flush(cx) {
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
        TransportState::FramedError(err) => Poll::Ready(Err(err)),
        TransportState::Stopping => Poll::Ready(Ok(())),
    }
}

fn poll_read<S, T, U>(
    cx: &mut Context<'_>,
    srv: &mut S,
    state: &mut TransportState<S, U>,
    framed: &mut Framed<T, U>,
    inner: &mut Inner<S, U>,
) -> bool
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
        match srv.poll_ready(cx) {
            Poll::Ready(Ok(_)) => {
                let item = match framed.next_item(cx) {
                    Poll::Ready(Some(Ok(el))) => el,
                    Poll::Ready(Some(Err(err))) => {
                        *state =
                            TransportState::FramedError(FramedTransportError::Decoder(err));
                        return true;
                    }
                    Poll::Pending => return false,
                    Poll::Ready(None) => {
                        *state = TransportState::Stopping;
                        return true;
                    }
                };

                let mut cell = inner.clone();
                let fut = srv.call(item).then(move |item| {
                    let inner = cell.get_mut();
                    inner.buf.push_back(item);
                    inner.task.wake();
                    ready(())
                });
                actix_rt::spawn(fut);
            }
            Poll::Pending => return false,
            Poll::Ready(Err(err)) => {
                *state = TransportState::Error(FramedTransportError::Service(err));
                return true;
            }
        }
    }
}

/// write to framed object
fn poll_write<S, T, U>(
    cx: &mut Context<'_>,
    state: &mut TransportState<S, U>,
    framed: &mut Framed<T, U>,
    rx: &mut Rx<U>,
    inner: &mut Inner<S, U>,
) -> bool
where
    S: Service<Request = Request<U>, Response = Response<U>>,
    S::Error: 'static,
    S::Future: 'static,
    T: AsyncRead + AsyncWrite,
    U: Decoder + Encoder,
    <U as Encoder>::Item: 'static,
    <U as Encoder>::Error: std::fmt::Debug,
{
    // let this = self.project();

    let inner = inner.get_mut();
    let mut rx_done = rx.is_none();
    let mut buf_empty = inner.buf.is_empty();
    loop {
        while !framed.is_write_buf_full() {
            if !buf_empty {
                match inner.buf.pop_front().unwrap() {
                    Ok(msg) => {
                        if let Err(err) = framed.write(msg) {
                            *state =
                                TransportState::FramedError(FramedTransportError::Encoder(err));
                            return true;
                        }
                        buf_empty = inner.buf.is_empty();
                    }
                    Err(err) => {
                        *state = TransportState::Error(FramedTransportError::Service(err));
                        return true;
                    }
                }
            }

            if !rx_done && rx.is_some() {
                match Pin::new(rx.as_mut().unwrap()).poll_next(cx) {
                    Poll::Ready(Some(FramedMessage::Message(msg))) => {
                        if let Err(err) = framed.write(msg) {
                            *state =
                                TransportState::FramedError(FramedTransportError::Encoder(err));
                            return true;
                        }
                    }
                    Poll::Ready(Some(FramedMessage::Close)) => {
                        *state = TransportState::FlushAndStop;
                        return true;
                    }
                    Poll::Ready(None) => {
                        rx_done = true;
                        let _ = rx.take();
                    }
                    Poll::Pending => rx_done = true,
                }
            }

            if rx_done && buf_empty {
                break;
            }
        }

        if !framed.is_write_buf_empty() {
            match framed.flush(cx) {
                Poll::Pending => break,
                Poll::Ready(Err(err)) => {
                    debug!("Error sending data: {:?}", err);
                    *state = TransportState::FramedError(FramedTransportError::Encoder(err));
                    return true;
                }
                Poll::Ready(Ok(_)) => (),
            }
        } else {
            break;
        }
    }

    false
}
