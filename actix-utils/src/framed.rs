//! Framed dispatcher service and related utilities
use std::collections::VecDeque;
use std::marker::PhantomData;
use std::mem;

use actix_codec::{AsyncRead, AsyncWrite, Decoder, Encoder, Framed};
use actix_service::{IntoNewService, IntoService, NewService, Service};
use futures::future::{ok, FutureResult};
use futures::task::AtomicTask;
use futures::{Async, Future, Poll, Sink, Stream};
use log::debug;

use crate::cell::Cell;

type Request<U> = <U as Decoder>::Item;
type Response<U> = <U as Encoder>::Item;

pub struct FramedNewService<S, T, U> {
    factory: S,
    _t: PhantomData<(T, U)>,
}

impl<S, T, U> FramedNewService<S, T, U>
where
    S: NewService<Request<U>, Response = Response<U>>,
    S::Service: 'static,
    T: AsyncRead + AsyncWrite + 'static,
    U: Decoder + Encoder + 'static,
    <U as Encoder>::Error: std::fmt::Debug,
{
    pub fn new<F1: IntoNewService<S, Request<U>>>(factory: F1) -> Self {
        Self {
            factory: factory.into_new_service(),
            _t: PhantomData,
        }
    }
}

impl<S, T, U> Clone for FramedNewService<S, T, U>
where
    S: Clone,
{
    fn clone(&self) -> Self {
        Self {
            factory: self.factory.clone(),
            _t: PhantomData,
        }
    }
}

impl<S, T, U> NewService<Framed<T, U>> for FramedNewService<S, T, U>
where
    S: NewService<Request<U>, Response = Response<U>> + Clone,
    S::Service: 'static,
    T: AsyncRead + AsyncWrite + 'static,
    U: Decoder + Encoder + 'static,
    <U as Encoder>::Error: std::fmt::Debug,
{
    type Response = FramedTransport<S::Service, T, U>;
    type Error = S::InitError;
    type InitError = S::InitError;
    type Service = FramedService<S, T, U>;
    type Future = FutureResult<Self::Service, Self::InitError>;

    fn new_service(&self) -> Self::Future {
        ok(FramedService {
            factory: self.factory.clone(),
            _t: PhantomData,
        })
    }
}

pub struct FramedService<S, T, U> {
    factory: S,
    _t: PhantomData<(T, U)>,
}

impl<S, T, U> Clone for FramedService<S, T, U>
where
    S: Clone,
{
    fn clone(&self) -> Self {
        Self {
            factory: self.factory.clone(),
            _t: PhantomData,
        }
    }
}

impl<S, T, U> Service<Framed<T, U>> for FramedService<S, T, U>
where
    S: NewService<Request<U>, Response = Response<U>>,
    S::Service: 'static,
    T: AsyncRead + AsyncWrite + 'static,
    U: Decoder + Encoder + 'static,
    <U as Encoder>::Error: std::fmt::Debug,
{
    type Response = FramedTransport<S::Service, T, U>;
    type Error = S::InitError;
    type Future = FramedServiceResponseFuture<S, T, U>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(Async::Ready(()))
    }

    fn call(&mut self, req: Framed<T, U>) -> Self::Future {
        FramedServiceResponseFuture {
            fut: self.factory.new_service(),

            framed: Some(req),
        }
    }
}

#[doc(hidden)]
pub struct FramedServiceResponseFuture<S, T, U>
where
    S: NewService<Request<U>, Response = Response<U>>,
    S::Service: 'static,
    T: AsyncRead + AsyncWrite + 'static,
    U: Decoder + Encoder + 'static,
    <U as Encoder>::Error: std::fmt::Debug,
{
    fut: S::Future,
    framed: Option<Framed<T, U>>,
}

impl<S, T, U> Future for FramedServiceResponseFuture<S, T, U>
where
    S: NewService<Request<U>, Response = Response<U>>,
    S::Service: 'static,
    T: AsyncRead + AsyncWrite + 'static,
    U: Decoder + Encoder + 'static,
    <U as Encoder>::Error: std::fmt::Debug,
{
    type Item = FramedTransport<S::Service, T, U>;
    type Error = S::InitError;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self.fut.poll()? {
            Async::NotReady => Ok(Async::NotReady),
            Async::Ready(service) => Ok(Async::Ready(FramedTransport::new(
                self.framed.take().unwrap(),
                service,
            ))),
        }
    }
}

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

/// FramedTransport - is a future that reads frames from Framed object
/// and pass then to the service.
pub struct FramedTransport<S, T, U>
where
    S: Service<Request<U>, Response = Response<U>> + 'static,
    T: AsyncRead + AsyncWrite + 'static,
    U: Encoder + Decoder + 'static,
    <U as Encoder>::Error: std::fmt::Debug,
{
    inner: Cell<FramedTransportInner<S, T, U>>,
    inner2: Cell<FramedTransportInner<S, T, U>>,
}

enum TransportState<S: Service<Request<U>>, U: Encoder + Decoder> {
    Processing,
    Error(FramedTransportError<S::Error, U>),
    FramedError(FramedTransportError<S::Error, U>),
    Stopping,
}

struct FramedTransportInner<S, T, U>
where
    S: Service<Request<U>, Response = Response<U>> + 'static,
    T: AsyncRead + AsyncWrite + 'static,
    U: Encoder + Decoder + 'static,
    <U as Encoder>::Error: std::fmt::Debug,
{
    service: S,
    state: TransportState<S, U>,
    framed: Framed<T, U>,
    buf: VecDeque<Result<Response<U>, S::Error>>,
    task: AtomicTask,
}

impl<S, T, U> FramedTransportInner<S, T, U>
where
    S: Service<Request<U>, Response = Response<U>> + 'static,
    T: AsyncRead + AsyncWrite + 'static,
    U: Decoder + Encoder + 'static,
    <U as Encoder>::Error: std::fmt::Debug,
{
    fn poll_service(&mut self, cell: &Cell<FramedTransportInner<S, T, U>>) -> bool {
        loop {
            match self.service.poll_ready() {
                Ok(Async::Ready(_)) => loop {
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

                    self.task.register();
                    let mut cell = cell.clone();
                    tokio_current_thread::spawn(self.service.call(item).then(move |item| {
                        let inner = cell.get_mut();
                        inner.buf.push_back(item);
                        inner.task.notify();
                        Ok(())
                    }));
                },
                Ok(Async::NotReady) => return false,
                Err(err) => {
                    self.state = TransportState::Error(FramedTransportError::Service(err));
                    return true;
                }
            }
        }
    }

    /// write to sink
    fn poll_response(&mut self) -> bool {
        loop {
            while !self.framed.is_write_buf_full() {
                if let Some(msg) = self.buf.pop_front() {
                    match msg {
                        Ok(msg) => {
                            if let Err(err) = self.framed.force_send(msg) {
                                self.state = TransportState::FramedError(
                                    FramedTransportError::Encoder(err),
                                );
                                return true;
                            }
                        }
                        Err(err) => {
                            self.state =
                                TransportState::Error(FramedTransportError::Service(err));
                            return true;
                        }
                    }
                } else {
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
    S: Service<Request<U>, Response = Response<U>> + 'static,
    T: AsyncRead + AsyncWrite + 'static,
    U: Decoder + Encoder + 'static,
    <U as Encoder>::Error: std::fmt::Debug,
{
    pub fn new<F: IntoService<S, Request<U>>>(framed: Framed<T, U>, service: F) -> Self {
        let inner = Cell::new(FramedTransportInner {
            framed,
            service: service.into_service(),
            state: TransportState::Processing,
            buf: VecDeque::new(),
            task: AtomicTask::new(),
        });

        FramedTransport {
            inner2: inner.clone(),
            inner,
        }
    }

    /// Get reference to a service wrapped by `FramedTransport` instance.
    pub fn get_ref(&self) -> &S {
        &self.inner.get_ref().service
    }

    /// Get mutable reference to a service wrapped by `FramedTransport`
    /// instance.
    pub fn get_mut(&mut self) -> &mut S {
        &mut self.inner.get_mut().service
    }

    /// Get reference to a framed instance wrapped by `FramedTransport`
    /// instance.
    pub fn get_framed(&self) -> &Framed<T, U> {
        &self.inner.get_ref().framed
    }

    /// Get mutable reference to a framed instance wrapped by `FramedTransport`
    /// instance.
    pub fn get_framed_mut(&mut self) -> &mut Framed<T, U> {
        &mut self.inner.get_mut().framed
    }
}

impl<S, T, U> Future for FramedTransport<S, T, U>
where
    S: Service<Request<U>, Response = Response<U>> + 'static,
    T: AsyncRead + AsyncWrite + 'static,
    U: Decoder + Encoder + 'static,
    <U as Encoder>::Error: std::fmt::Debug,
{
    type Item = ();
    type Error = FramedTransportError<S::Error, U>;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let inner = self.inner.get_mut();

        match mem::replace(&mut inner.state, TransportState::Processing) {
            TransportState::Processing => {
                if inner.poll_service(&self.inner2) || inner.poll_response() {
                    self.poll()
                } else {
                    Ok(Async::NotReady)
                }
            }
            TransportState::Error(err) => {
                if inner.framed.is_write_buf_empty()
                    || (inner.poll_response() || inner.framed.is_write_buf_empty())
                {
                    Err(err)
                } else {
                    inner.state = TransportState::Error(err);
                    Ok(Async::NotReady)
                }
            }
            TransportState::FramedError(err) => Err(err),
            TransportState::Stopping => Ok(Async::Ready(())),
        }
    }
}

pub struct IntoFramed<T, U, F>
where
    T: AsyncRead + AsyncWrite,
    F: Fn() -> U + Send + Clone + 'static,
    U: Encoder + Decoder,
{
    factory: F,
    _t: PhantomData<(T,)>,
}

impl<T, U, F> IntoFramed<T, U, F>
where
    T: AsyncRead + AsyncWrite,
    F: Fn() -> U + Send + Clone + 'static,
    U: Encoder + Decoder,
{
    pub fn new(factory: F) -> Self {
        IntoFramed {
            factory,
            _t: PhantomData,
        }
    }
}

impl<T, U, F> NewService<T> for IntoFramed<T, U, F>
where
    T: AsyncRead + AsyncWrite,
    F: Fn() -> U + Send + Clone + 'static,
    U: Encoder + Decoder,
{
    type Response = Framed<T, U>;
    type Error = ();
    type InitError = ();
    type Service = IntoFramedService<T, U, F>;
    type Future = FutureResult<Self::Service, Self::InitError>;

    fn new_service(&self) -> Self::Future {
        ok(IntoFramedService {
            factory: self.factory.clone(),
            _t: PhantomData,
        })
    }
}

pub struct IntoFramedService<T, U, F>
where
    T: AsyncRead + AsyncWrite,
    F: Fn() -> U + Send + Clone + 'static,
    U: Encoder + Decoder,
{
    factory: F,
    _t: PhantomData<(T,)>,
}

impl<T, U, F> Service<T> for IntoFramedService<T, U, F>
where
    T: AsyncRead + AsyncWrite,
    F: Fn() -> U + Send + Clone + 'static,
    U: Encoder + Decoder,
{
    type Response = Framed<T, U>;
    type Error = ();
    type Future = FutureResult<Self::Response, Self::Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(Async::Ready(()))
    }

    fn call(&mut self, req: T) -> Self::Future {
        ok(Framed::new(req, (self.factory)()))
    }
}
