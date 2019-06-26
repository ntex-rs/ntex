//! Framed dispatcher service and related utilities
use std::collections::VecDeque;
use std::{fmt, mem};

use actix_codec::{AsyncRead, AsyncWrite, Decoder, Encoder, Framed};
use actix_service::{IntoService, Service};
use futures::task::AtomicTask;
use futures::unsync::mpsc;
use futures::{Async, Future, Poll, Sink, Stream};
use log::debug;

use crate::cell::Cell;

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
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
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
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
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

/// FramedTransport - is a future that reads frames from Framed object
/// and pass then to the service.
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
    rx: Option<mpsc::UnboundedReceiver<FramedMessage<<U as Encoder>::Item>>>,
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
    task: AtomicTask,
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
    fn poll_read(&mut self) -> bool {
        loop {
            match self.service.poll_ready() {
                Ok(Async::Ready(_)) => {
                    let item = match self.framed.poll() {
                        Ok(Async::Ready(Some(el))) => el,
                        Err(err) => {
                            self.state =
                                TransportState::FramedError(FramedTransportError::Decoder(err));
                            return true;
                        }
                        Ok(Async::NotReady) => return false,
                        Ok(Async::Ready(None)) => {
                            self.state = TransportState::Stopping;
                            return true;
                        }
                    };

                    let mut cell = self.inner.clone();
                    cell.get_mut().task.register();
                    tokio_current_thread::spawn(self.service.call(item).then(move |item| {
                        let inner = cell.get_mut();
                        inner.buf.push_back(item);
                        inner.task.notify();
                        Ok(())
                    }));
                }
                Ok(Async::NotReady) => return false,
                Err(err) => {
                    self.state = TransportState::Error(FramedTransportError::Service(err));
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
                                self.state = TransportState::FramedError(
                                    FramedTransportError::Encoder(err),
                                );
                                return true;
                            }
                            buf_empty = inner.buf.is_empty();
                        }
                        Err(err) => {
                            self.state =
                                TransportState::Error(FramedTransportError::Service(err));
                            return true;
                        }
                    }
                }

                if !rx_done && self.rx.is_some() {
                    match self.rx.as_mut().unwrap().poll() {
                        Ok(Async::Ready(Some(FramedMessage::Message(msg)))) => {
                            if let Err(err) = self.framed.force_send(msg) {
                                self.state = TransportState::FramedError(
                                    FramedTransportError::Encoder(err),
                                );
                                return true;
                            }
                        }
                        Ok(Async::Ready(Some(FramedMessage::Close))) => {
                            self.state = TransportState::FlushAndStop;
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
                        self.state =
                            TransportState::FramedError(FramedTransportError::Encoder(err));
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
                task: AtomicTask::new(),
            }),
        }
    }

    /// Get Sender
    pub fn set_receiver(
        mut self,
        rx: mpsc::UnboundedReceiver<FramedMessage<<U as Encoder>::Item>>,
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
{
    type Item = ();
    type Error = FramedTransportError<S::Error, U>;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match mem::replace(&mut self.state, TransportState::Processing) {
            TransportState::Processing => {
                if self.poll_read() || self.poll_write() {
                    self.poll()
                } else {
                    Ok(Async::NotReady)
                }
            }
            TransportState::Error(err) => {
                if self.framed.is_write_buf_empty()
                    || (self.poll_write() || self.framed.is_write_buf_empty())
                {
                    Err(err)
                } else {
                    self.state = TransportState::Error(err);
                    Ok(Async::NotReady)
                }
            }
            TransportState::FlushAndStop => {
                if !self.framed.is_write_buf_empty() {
                    match self.framed.poll_complete() {
                        Err(err) => {
                            debug!("Error sending data: {:?}", err);
                            Ok(Async::Ready(()))
                        }
                        Ok(Async::NotReady) => Ok(Async::NotReady),
                        Ok(Async::Ready(_)) => Ok(Async::Ready(())),
                    }
                } else {
                    Ok(Async::Ready(()))
                }
            }
            TransportState::FramedError(err) => Err(err),
            TransportState::Stopping => Ok(Async::Ready(())),
        }
    }
}
